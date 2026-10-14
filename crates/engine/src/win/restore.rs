//! Restoring images to physical disks and partitions, and exporting them as VHDX files.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blockdev::gpt::{self, Gpt};
use blockdev::mbr::{self, MbrEntry};
use blockdev::{BlockDevice, Slice};
use irimg::PartitionStyle;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use winio::{DiskLayout, LayoutStyle, PartitionInfo, PartitionKind, RawDisk, VirtualDisk, Volume};

use super::{copy_device, disks_holding, refuse_system_disk, volumes_holding, AnyImage};
use crate::progress::{Cancel, ProgressFn, Reporter};
use crate::restore::{restore_disk, restore_partition, RestoreOptions, ZeroFill};
use crate::{Error, Result};

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskRestoreOptions {
    /// Give the restored disk a new MBR signature / GPT GUIDs. `None` does so only when another
    /// disk of this computer already has the image's identity (Windows would take one of them
    /// offline). Ignored for partition restores.
    pub rewrite_identity: Option<bool>,
    /// Grow the last partition (or the restored partition) and its file system to the end of
    /// the free space.
    pub extend: bool,
    /// Decoder threads; 0 picks one per core.
    pub threads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRestoreReport {
    pub target_disk: u32,
    /// The partition written, for partition restores.
    pub partition: Option<u32>,
    pub bytes_written: u64,
    pub identity_rewritten: bool,
    /// A partition and its file system were grown.
    pub extended: bool,
    pub warnings: Vec<String>,
}

/// Where a partition is restored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PartitionTarget {
    /// Overwrite an existing partition at least as large as the imaged one.
    Existing { disk: u32, number: u32 },
    /// Create a new partition in unallocated space (the first region that fits, or the region
    /// starting at `offset`).
    FreeSpace { disk: u32, offset: Option<u64> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Identity {
    Mbr(u32),
    Gpt(Uuid),
}

pub(super) fn read_head(dev: &dyn BlockDevice) -> Result<Vec<u8>> {
    let mut head = vec![0u8; dev.size().min(MIB) as usize];
    dev.read_at(0, &mut head)?;
    Ok(head)
}

fn head_identity(head: &[u8], ss: u32) -> Option<Identity> {
    if let Some((h, _)) = gpt::parse_primary(head, ss) {
        return Some(Identity::Gpt(h.disk_id));
    }
    (head.len() >= 512 && mbr::has_boot_signature(head)).then(|| Identity::Mbr(mbr::signature(head)))
}

fn image_identity(image: &AnyImage, disk: u32) -> Result<Option<Identity>> {
    Ok(match image {
        AnyImage::Irimg(r) => match r.manifest().disk(disk).map(|d| &d.layout) {
            Some(PartitionStyle::Mbr { signature }) => Some(Identity::Mbr(*signature)),
            Some(PartitionStyle::Gpt { disk_id, .. }) => Some(Identity::Gpt(*disk_id)),
            _ => None,
        },
        AnyImage::Mrimgx(m) => {
            let dev = m.disk_device(disk as usize)?;
            head_identity(&read_head(&*dev)?, dev.block_size())
        }
    })
}

fn layout_identity(l: &DiskLayout) -> Option<Identity> {
    match l.style {
        LayoutStyle::Mbr { signature } => Some(Identity::Mbr(signature)),
        LayoutStyle::Gpt { disk_id, .. } => Some(Identity::Gpt(disk_id)),
        LayoutStyle::Raw => None,
    }
}

/// Whether a disk other than `except` has identity `id`.
fn identity_in_use(id: Identity, except: u32) -> Result<bool> {
    if id == Identity::Mbr(0) {
        return Ok(false); // Windows assigns a signature to such a disk itself
    }
    Ok(winio::list_disks()?.iter().any(|d| d.number != except && layout_identity(&d.layout) == Some(id)))
}

/// Partition table fix-ups for a disk image written to a larger (or differently identified)
/// device.
pub(super) struct Retable {
    /// Byte offset and data to write.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// Offset and new length of the partition grown to the end of the disk.
    pub extended: Option<(u64, u64)>,
}

/// Rewrites the partition table found in `head` (the first bytes of a disk image of
/// `image_size` bytes) for a device of `size` bytes: moves the backup GPT to the end, gives
/// the disk new identifiers when `rewrite` is set and grows the last partition when `extend`
/// is set. Returns no writes when nothing changes.
pub(super) fn retable(head: &[u8], ss: u32, image_size: u64, size: u64, rewrite: bool, extend: bool) -> Retable {
    let s = ss as u64;
    let mut out = Retable { writes: Vec::new(), extended: None };
    if !(512..=65536).contains(&ss) || !ss.is_power_of_two() {
        return out;
    }
    let extend = extend && size > image_size;

    if let Some((h, mut entries)) = gpt::parse_primary(head, ss) {
        let mut disk_id = h.disk_id;
        if rewrite {
            disk_id = Uuid::new_v4();
            for e in &mut entries {
                e.id = Uuid::new_v4();
            }
        }
        let mut g = Gpt { disk_id, sector_size: ss, entry_count: h.entry_count, entries };
        if extend {
            let Some(last_usable) = (size / s).checked_sub(2 + g.entry_array_sectors()) else { return out };
            // Exclusive end LBA, 1 MiB aligned.
            let end = (last_usable + 1) * s / MIB * MIB / s;
            if let Some(e) = g.entries.iter_mut().max_by_key(|e| e.last_lba) {
                if end > e.last_lba.saturating_add(1) && e.first_lba < end {
                    e.last_lba = end - 1;
                    out.extended = Some((e.first_lba * s, (end - e.first_lba) * s));
                }
            }
        }
        if !rewrite && out.extended.is_none() && size == image_size {
            return out;
        }
        // The original protective (or hybrid) MBR only describes a disk of the original size.
        let pmbr = (size == image_size && head.len() >= 512).then(|| &head[..512]);
        let mut sectors = g.build(size, None, pmbr);
        // Preserve the header boundary without allocating the unused space before it.
        let first = h.first_usable_lba.max(sectors.first_usable_lba);
        for header in [
            &mut sectors.primary[s as usize..2 * s as usize],
            &mut sectors.backup[(g.entry_array_sectors() * s) as usize..],
        ] {
            let mut h = gpt::GptHeader::decode(header).expect("newly built GPT header");
            h.first_usable_lba = first;
            header.copy_from_slice(&h.encode(ss));
        }
        let keep = (((2 + g.entry_array_sectors()) * s) as usize).min(sectors.primary.len());
        out.writes.push((0, sectors.primary[..keep].to_vec()));
        out.writes.push((sectors.backup_offset, sectors.backup));
        return out;
    }

    if head.len() >= 512 && mbr::has_boot_signature(head) {
        let mut sector = head[..512].to_vec();
        let mut changed = false;
        if rewrite {
            mbr::set_signature(&mut sector, mbr::random_signature());
            changed = true;
        }
        if extend {
            let last = mbr::entries(&sector)
                .into_iter()
                .enumerate()
                .filter_map(|(i, e)| e.map(|e| (i, e)))
                .max_by_key(|(_, e)| e.first_lba as u64 + e.sectors as u64);
            if let Some((i, e)) = last.filter(|(_, e)| !mbr::is_extended(e.partition_type)) {
                let first = e.first_lba as u64;
                let end = (size / MIB * MIB / s).min(first + u32::MAX as u64);
                if end > first + e.sectors as u64 {
                    let grown = MbrEntry { sectors: (end - first) as u32, ..e };
                    mbr::set_entry(&mut sector, i, Some(&grown));
                    out.extended = Some((first * s, grown.sectors as u64 * s));
                    changed = true;
                }
            }
        }
        if changed {
            out.writes.push((0, sector));
        }
    }
    out
}

/// A read-only view of a device with some byte ranges replaced.
pub(super) struct Patched {
    inner: Arc<dyn BlockDevice>,
    /// Sorted by offset and not overlapping.
    patches: Vec<(u64, Vec<u8>)>,
}

impl Patched {
    /// Where patches overlap, later ones win.
    pub fn new(inner: Arc<dyn BlockDevice>, patches: Vec<(u64, Vec<u8>)>) -> Self {
        let mut sorted: Vec<(u64, Vec<u8>)> = Vec::with_capacity(patches.len());
        for (at, data) in patches.into_iter().filter(|(_, d)| !d.is_empty()) {
            // Usually disjoint; otherwise merge the overlapping ones into a single range.
            let end = at + data.len() as u64;
            let first = sorted.partition_point(|(a, d)| a + d.len() as u64 <= at);
            let last = sorted.partition_point(|(a, _)| *a < end);
            if first == last {
                sorted.insert(first, (at, data));
                continue;
            }
            let start = at.min(sorted[first].0);
            let stop = end.max(sorted[last - 1].0 + sorted[last - 1].1.len() as u64);
            let mut merged = vec![0u8; (stop - start) as usize];
            for (a, d) in sorted.drain(first..last).chain(std::iter::once((at, data))) {
                merged[(a - start) as usize..][..d.len()].copy_from_slice(&d);
            }
            sorted.insert(first, (start, merged));
        }
        Self { inner, patches: sorted }
    }
}

impl BlockDevice for Patched {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(offset, buf)?;
        let end = offset + buf.len() as u64;
        let first = self.patches.partition_point(|(at, data)| at + data.len() as u64 <= offset);
        for (at, data) in self.patches[first..].iter().take_while(|(at, _)| *at < end) {
            let (a, b) = (offset.max(*at), end.min(at + data.len() as u64));
            buf[(a - offset) as usize..(b - offset) as usize]
                .copy_from_slice(&data[(a - at) as usize..(b - at) as usize]);
        }
        Ok(())
    }
    fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "device is read-only"))
    }
}

/// Opens, locks and dismounts a volume, retrying the lock briefly while handles close.
fn lock_volume(guid_path: &str) -> Result<Volume> {
    let volume = Volume::open(guid_path, true)?;
    let mut result = volume.lock();
    for _ in 0..8 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        result = volume.lock();
    }
    result.map_err(|e| Error::invalid(format!("the volume is in use and could not be locked: {e}")))?;
    volume.dismount()?;
    Ok(volume)
}

/// Grows the file system of the partition at `offset` to `len` bytes once Windows has
/// mounted it.
fn extend_filesystem(disk: u32, offset: u64, len: u64, ss: u32) -> Result<()> {
    let start = Instant::now();
    let guid = loop {
        if let Some(g) = winio::volume_for_partition(disk, offset)? {
            break g;
        }
        if start.elapsed() > Duration::from_secs(30) {
            return Err(Error::invalid("the volume did not appear"));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    Volume::open(&guid, true)?.extend(len / ss as u64)?;
    Ok(())
}

/// Restores image disk `disk` onto physical disk `target_disk`, which must be at least as
/// large, use the same sector size and neither run Windows nor hold the image.
pub fn restore_image_disk(
    image: &AnyImage,
    image_path: &Path,
    disk: u32,
    target_disk: u32,
    opts: &DiskRestoreOptions,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<DiskRestoreReport> {
    let src = image.disk(disk).ok_or_else(|| Error::invalid(format!("the image has no disk {disk}")))?;
    let info = winio::disk_info(target_disk)?;
    refuse_system_disk(&info)?;
    if disks_holding(&image.files(image_path))?.contains(&target_disk) {
        return Err(Error::invalid(format!("disk {target_disk} holds the image being restored")));
    }
    if info.size < src.size {
        return Err(Error::invalid(format!(
            "disk {target_disk} ({} bytes) is smaller than the imaged disk ({} bytes); restoring to a smaller \
             disk is not supported",
            info.size, src.size
        )));
    }
    if info.sector_size != src.sector_size {
        return Err(Error::invalid(format!(
            "disk {target_disk} has {}-byte sectors, the image {}-byte sectors",
            info.sector_size, src.sector_size
        )));
    }
    let rewrite = match opts.rewrite_identity {
        Some(r) => r,
        None => match image_identity(image, disk)? {
            Some(id) => identity_in_use(id, target_disk)?,
            None => false,
        },
    };
    let _ = winio::enable_privileges(&["SeManageVolumePrivilege"]);

    let raw = RawDisk::open(target_disk, true)?;
    if info.read_only {
        raw.set_attributes(None, Some(false))?;
    }
    let locks = raw.lock_and_dismount_volumes()?;
    // Offline, Windows does not mount the half-written partitions.
    let _ = raw.set_attributes(Some(true), None);

    let result = (|| -> Result<(u64, Option<(u64, u64)>)> {
        match image {
            AnyImage::Irimg(r) => {
                let ropts = RestoreOptions {
                    rewrite_identity: rewrite,
                    zero_fill: ZeroFill::Auto,
                    extend_last: opts.extend,
                    threads: opts.threads,
                };
                let report = restore_disk(r, disk, &raw, &ropts, progress, cancel)?;
                let extended = report.extended.and_then(|n| report.layout.partition(n)).map(|p| (p.offset, p.length));
                Ok((report.bytes_written, extended))
            }
            AnyImage::Mrimgx(m) => {
                let dev = m.disk_device(disk as usize)?;
                let mut rep = Reporter::new(progress, "restore", src.size);
                let mut written = copy_device(&*dev, &raw, src.size, false, &mut rep, cancel)?;
                let t = retable(&read_head(&*dev)?, src.sector_size, src.size, info.size, rewrite, opts.extend);
                for (offset, data) in &t.writes {
                    raw.write_at(*offset, data)?;
                    written += data.len() as u64;
                }
                raw.flush()?;
                Ok((written, t.extended))
            }
        }
    })();

    drop(locks);
    if !info.offline {
        let _ = raw.set_attributes(Some(false), None);
    }
    let _ = raw.update_properties();
    let (bytes_written, extended) = result?;

    let mut warnings = Vec::new();
    let mut did_extend = false;
    if let Some((offset, len)) = extended {
        if info.offline {
            warnings.push(format!(
                "disk {target_disk} was offline; bring it online and extend the last partition's file system manually"
            ));
        } else {
            match extend_filesystem(target_disk, offset, len, info.sector_size) {
                Ok(()) => did_extend = true,
                Err(e) => warnings
                    .push(format!("the last partition was grown but its file system could not be extended: {e}")),
            }
        }
    }
    Ok(DiskRestoreReport {
        target_disk,
        partition: None,
        bytes_written,
        identity_rewritten: rewrite,
        extended: did_extend,
        warnings,
    })
}

/// The length and partition type of an imaged partition.
fn source_partition(image: &AnyImage, disk: u32, number: u32) -> Result<(u64, Option<PartitionKind>)> {
    let missing = || Error::invalid(format!("partition {number} of disk {disk} is not in the image"));
    match image {
        AnyImage::Irimg(r) => {
            let d = r.manifest().disk(disk).ok_or_else(missing)?;
            let p = d.partition(number).filter(|p| p.captured).ok_or_else(missing)?;
            let kind = match (&p.gpt, &p.mbr) {
                (Some(g), _) => Some(PartitionKind::Gpt {
                    type_id: g.type_id,
                    id: g.id,
                    name: g.name.clone(),
                    attributes: g.attributes,
                }),
                (None, Some(m)) => {
                    Some(PartitionKind::Mbr { partition_type: m.partition_type, bootable: m.bootable, logical: false })
                }
                (None, None) => None,
            };
            Ok((p.length, kind))
        }
        AnyImage::Mrimgx(m) => {
            let d = m.info().disks.get(disk as usize).ok_or_else(missing)?;
            let p = d.partitions.iter().find(|p| p.number == number).ok_or_else(missing)?;
            let kind = match (p.gpt_type.as_deref().and_then(|t| Uuid::parse_str(t).ok()), p.mbr_type) {
                (Some(type_id), _) => {
                    Some(PartitionKind::Gpt { type_id, id: Uuid::new_v4(), name: String::new(), attributes: 0 })
                }
                (None, Some(t)) => Some(PartitionKind::Mbr { partition_type: t, bootable: false, logical: false }),
                (None, None) => None,
            };
            Ok((p.length, kind))
        }
    }
}

/// The partition type to give a restored partition on a disk of the given style.
fn kind_for(src: Option<&PartitionKind>, gpt_target: bool) -> PartitionKind {
    if gpt_target {
        let (type_id, name, attributes) = match src {
            Some(PartitionKind::Gpt { type_id, name, attributes, .. }) => (*type_id, name.clone(), *attributes),
            Some(PartitionKind::Mbr { partition_type: 0xEF, .. }) => (gpt::EFI_SYSTEM, String::new(), 0),
            Some(PartitionKind::Mbr { partition_type: 0x27, .. }) => (gpt::WINDOWS_RECOVERY, String::new(), 0),
            _ => (gpt::BASIC_DATA, String::new(), 0),
        };
        PartitionKind::Gpt { type_id, id: Uuid::new_v4(), name, attributes }
    } else {
        let partition_type = match src {
            Some(PartitionKind::Mbr { partition_type, .. }) => *partition_type,
            Some(PartitionKind::Gpt { type_id, .. }) if *type_id == gpt::EFI_SYSTEM => 0xEF,
            Some(PartitionKind::Gpt { type_id, .. }) if *type_id == gpt::WINDOWS_RECOVERY => 0x27,
            _ => 0x07,
        };
        PartitionKind::Mbr { partition_type, bootable: false, logical: false }
    }
}

fn same_type(a: &PartitionKind, b: &PartitionKind) -> bool {
    match (a, b) {
        (PartitionKind::Gpt { type_id: x, .. }, PartitionKind::Gpt { type_id: y, .. }) => x == y,
        (PartitionKind::Mbr { partition_type: x, .. }, PartitionKind::Mbr { partition_type: y, .. }) => x == y,
        _ => false,
    }
}

/// Writes the imaged partition to `target` (offset 0 of the target is the partition start).
#[allow(clippy::too_many_arguments)]
fn write_partition(
    image: &AnyImage,
    disk: u32,
    number: u32,
    len: u64,
    target: &dyn BlockDevice,
    threads: usize,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<u64> {
    match image {
        AnyImage::Irimg(r) => {
            let ropts = RestoreOptions { threads, ..Default::default() };
            restore_partition(r, disk, number, target, &ropts, progress, cancel)
        }
        AnyImage::Mrimgx(m) => {
            let d = &m.info().disks[disk as usize];
            let index = d.partitions.iter().find(|p| p.number == number).map(|p| p.index).unwrap_or_default();
            let dev = m.partition_device(disk as usize, index)?;
            let mut rep = Reporter::new(progress, "restore", len);
            copy_device(&*dev, target, len, false, &mut rep, cancel)
        }
    }
}

/// Restores partition `number` of image disk `disk` over an existing partition or into free
/// space of a physical disk.
#[allow(clippy::too_many_arguments)]
pub fn restore_image_partition(
    image: &AnyImage,
    image_path: &Path,
    disk: u32,
    number: u32,
    target: PartitionTarget,
    opts: &DiskRestoreOptions,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<DiskRestoreReport> {
    let (src_len, src_kind) = source_partition(image, disk, number)?;
    let target_disk = match target {
        PartitionTarget::Existing { disk, .. } | PartitionTarget::FreeSpace { disk, .. } => disk,
    };
    let info = winio::disk_info(target_disk)?;
    let ss = info.sector_size as u64;
    let gpt_target = matches!(info.layout.style, LayoutStyle::Gpt { .. });
    let _ = winio::enable_privileges(&["SeManageVolumePrivilege"]);
    let mut warnings = Vec::new();

    let (number_written, offset, len, bytes_written) = match target {
        PartitionTarget::Existing { number: target_number, .. } => {
            let tp = info
                .layout
                .partitions
                .iter()
                .find(|p| p.number == target_number)
                .cloned()
                .ok_or_else(|| Error::invalid(format!("disk {target_disk} has no partition {target_number}")))?;
            if let Some(v) = &tp.volume {
                if v.is_windows || v.is_system {
                    return Err(Error::invalid(format!(
                        "partition {target_number} of disk {target_disk} is in use by the running Windows \
                         installation"
                    )));
                }
                if volumes_holding(&image.files(image_path))?.iter().any(|g| g.eq_ignore_ascii_case(&v.guid_path)) {
                    return Err(Error::invalid(format!(
                        "partition {target_number} of disk {target_disk} holds the image being restored"
                    )));
                }
            }
            if tp.length < src_len {
                return Err(Error::invalid(format!(
                    "partition {target_number} of disk {target_disk} ({} bytes) is smaller than the imaged \
                     partition ({src_len} bytes)",
                    tp.length
                )));
            }

            let raw = Arc::new(RawDisk::open(target_disk, true)?);
            if info.read_only {
                raw.set_attributes(None, Some(false))?;
            }
            let lock = match &tp.volume {
                Some(v) => Some(lock_volume(&v.guid_path)?),
                None => None,
            };
            let slice = Slice::new(raw.clone(), tp.offset, tp.length)?;
            let written = write_partition(image, disk, number, src_len, &slice, opts.threads, progress, cancel)?;

            let mut want = kind_for(src_kind.as_ref(), gpt_target);
            if !same_type(&tp.kind, &want) {
                let has_logical =
                    info.layout.partitions.iter().any(|p| matches!(p.kind, PartitionKind::Mbr { logical: true, .. }));
                if has_logical {
                    warnings.push(format!(
                        "the partition type of partition {target_number} was left unchanged (the disk has logical \
                         partitions)"
                    ));
                } else {
                    // Keep the target's identity and boot flag; only the type changes.
                    match (&mut want, &tp.kind) {
                        (PartitionKind::Gpt { id, .. }, PartitionKind::Gpt { id: old, .. }) => *id = *old,
                        (PartitionKind::Mbr { bootable, .. }, PartitionKind::Mbr { bootable: old, .. }) => {
                            *bootable = *old
                        }
                        _ => {}
                    }
                    let mut layout = info.layout.clone();
                    for p in &mut layout.partitions {
                        if p.number == target_number {
                            p.kind = want.clone();
                        }
                    }
                    raw.set_layout(&layout)?;
                }
            }
            let _ = raw.update_properties();
            drop(lock);
            (target_number, tp.offset, tp.length, written)
        }
        PartitionTarget::FreeSpace { offset: wanted, .. } => {
            let (lo, hi) = match &info.layout.style {
                LayoutStyle::Raw => {
                    return Err(Error::invalid(format!(
                        "disk {target_disk} has no partition table; initialize it first"
                    )))
                }
                LayoutStyle::Gpt { starting_usable_offset, usable_length, .. } => {
                    (*starting_usable_offset, starting_usable_offset + usable_length)
                }
                LayoutStyle::Mbr { .. } => {
                    if info.layout.partitions.iter().any(|p| matches!(p.kind, PartitionKind::Mbr { logical: true, .. }))
                    {
                        return Err(Error::invalid(format!(
                            "disk {target_disk} has logical partitions; restore over an existing partition instead"
                        )));
                    }
                    if info.layout.partitions.len() >= 4 {
                        return Err(Error::invalid(format!(
                            "all four primary partition slots of disk {target_disk} are in use"
                        )));
                    }
                    (MIB, info.size.min(u32::MAX as u64 * ss))
                }
            };
            let mut used: Vec<(u64, u64)> =
                info.layout.partitions.iter().map(|p| (p.offset, p.offset + p.length)).collect();
            used.sort_unstable();
            let mut free = Vec::new();
            let mut pos = lo;
            for (a, b) in used {
                if a > pos {
                    free.push((pos, a.min(hi)));
                }
                pos = pos.max(b);
            }
            if hi > pos {
                free.push((pos, hi));
            }
            let (start, end) = match wanted {
                Some(o) => {
                    if o % ss != 0 {
                        return Err(Error::invalid(format!("offset {o} is not a multiple of the sector size")));
                    }
                    let r = free.iter().find(|(a, b)| o >= *a && o < *b).ok_or_else(|| {
                        Error::invalid(format!("offset {o} of disk {target_disk} is not unallocated space"))
                    })?;
                    (o, r.1)
                }
                None => free
                    .iter()
                    .map(|&(a, b)| (a.div_ceil(MIB) * MIB, b))
                    .find(|&(a, b)| a < b && b - a >= src_len)
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "disk {target_disk} has no unallocated region of at least {src_len} bytes"
                        ))
                    })?,
            };
            if end - start < src_len {
                return Err(Error::invalid(format!(
                    "the unallocated region at offset {start} ({} bytes) is smaller than the imaged partition \
                     ({src_len} bytes)",
                    end - start
                )));
            }
            let src_end = start + src_len.div_ceil(ss) * ss;
            let end = if !opts.extend {
                src_end
            } else if end / MIB * MIB >= src_end {
                end / MIB * MIB
            } else {
                end / ss * ss
            };
            let len = end - start;

            let raw = Arc::new(RawDisk::open(target_disk, true)?);
            if info.read_only {
                raw.set_attributes(None, Some(false))?;
            }
            let slice = Slice::new(raw.clone(), start, len)?;
            let written = write_partition(image, disk, number, src_len, &slice, opts.threads, progress, cancel)?;

            let new_number = if gpt_target {
                info.layout.partitions.iter().map(|p| p.number).max().unwrap_or(0) + 1
            } else {
                (1..=4).find(|n| !info.layout.partitions.iter().any(|p| p.number == *n)).unwrap_or(4)
            };
            let mut layout = info.layout.clone();
            layout.partitions.push(PartitionInfo {
                number: new_number,
                offset: start,
                length: len,
                kind: kind_for(src_kind.as_ref(), gpt_target),
                volume: None,
            });
            raw.set_layout(&layout)?;
            (new_number, start, len, written)
        }
    };

    let mut extended = false;
    if opts.extend && len > src_len {
        if info.offline {
            warnings.push(format!(
                "disk {target_disk} is offline; bring it online and extend the partition's file system manually"
            ));
        } else {
            match extend_filesystem(target_disk, offset, len, info.sector_size) {
                Ok(()) => extended = true,
                Err(e) => warnings.push(format!("the file system could not be extended: {e}")),
            }
        }
    }
    Ok(DiskRestoreReport {
        target_disk,
        partition: Some(number_written),
        bytes_written,
        identity_rewritten: false,
        extended,
        warnings,
    })
}

/// Writes image disk `disk` into a new dynamic VHDX at `out`. Returns the bytes written.
pub fn export_vhdx(image: &AnyImage, disk: u32, out: &Path, progress: ProgressFn, cancel: &Cancel) -> Result<u64> {
    let src = image.disk(disk).ok_or_else(|| Error::invalid(format!("the image has no disk {disk}")))?;
    if !matches!(src.sector_size, 512 | 4096) {
        return Err(Error::invalid(format!("VHDX does not support {}-byte sectors", src.sector_size)));
    }
    if out.exists() {
        return Err(Error::invalid(format!("{} already exists", out.display())));
    }
    let physical = if src.physical_sector_size == 4096 { 4096 } else { 512 }.max(src.sector_size);
    let size = src.size.div_ceil(MIB) * MIB;
    let vhd = VirtualDisk::create_vhdx(out, size, src.sector_size, physical, true)?;

    let result = (|| -> Result<u64> {
        vhd.attach(false, true)?;
        let raw = RawDisk::open(vhd.disk_number()?, true)?;
        let _ = raw.set_attributes(Some(true), None);
        let written = match image {
            AnyImage::Irimg(r) => {
                let ropts = RestoreOptions { zero_fill: ZeroFill::Never, ..Default::default() };
                restore_disk(r, disk, &raw, &ropts, progress, cancel)?.bytes_written
            }
            AnyImage::Mrimgx(m) => {
                let dev = m.disk_device(disk as usize)?;
                let mut rep = Reporter::new(progress, "export", src.size);
                let mut written = copy_device(&*dev, &raw, src.size, true, &mut rep, cancel)?;
                for (offset, data) in retable(&read_head(&*dev)?, src.sector_size, src.size, size, false, false).writes
                {
                    raw.write_at(offset, &data)?;
                    written += data.len() as u64;
                }
                raw.flush()?;
                written
            }
        };
        drop(raw);
        vhd.detach()?;
        Ok(written)
    })();

    if result.is_err() {
        let _ = vhd.detach();
        drop(vhd);
        let _ = std::fs::remove_file(out);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockdev::gpt::{GptEntry, DEFAULT_ENTRY_COUNT};
    use blockdev::MemDevice;

    fn gpt_head(size: u64) -> (Vec<u8>, Uuid) {
        let g = Gpt {
            disk_id: Uuid::new_v4(),
            sector_size: 512,
            entry_count: DEFAULT_ENTRY_COUNT,
            entries: vec![GptEntry {
                type_id: gpt::BASIC_DATA,
                id: Uuid::new_v4(),
                first_lba: 2048,
                last_lba: 4095,
                attributes: 0,
                name: "data".into(),
            }],
        };
        let mut head = g.build(size, Some(2048), None).primary;
        head.resize(MIB as usize, 0);
        (head, g.disk_id)
    }

    #[test]
    fn retable_gpt_grow_and_rewrite() {
        let (head, id) = gpt_head(4 * MIB);
        assert!(retable(&head, 512, 4 * MIB, 4 * MIB, false, false).writes.is_empty());

        let t = retable(&head, 512, 4 * MIB, 16 * MIB, true, true);
        assert_eq!(t.extended, Some((MIB, 14 * MIB)));
        let dev = MemDevice::new(16 * MIB, 512);
        for (o, d) in &t.writes {
            dev.write_at(*o, d).unwrap();
        }
        let (h, e) = gpt::parse_primary(&read_head(&dev).unwrap(), 512).unwrap();
        assert_ne!(h.disk_id, id);
        assert_eq!(h.alternate_lba, 16 * MIB / 512 - 1);
        assert_eq!(e[0].last_lba, 15 * MIB / 512 - 1);
    }

    #[test]
    fn retable_mbr_grow() {
        let mut head = mbr::build(
            0x1234,
            &[MbrEntry { bootable: true, partition_type: 0x07, first_lba: 2048, sectors: 2048 }],
            None,
        )
        .to_vec();
        head.resize(MIB as usize, 0);
        let t = retable(&head, 512, 4 * MIB, 8 * MIB, false, true);
        assert_eq!(t.extended, Some((MIB, 7 * MIB)));
        let e = mbr::entries(&t.writes[0].1)[0].unwrap();
        assert_eq!(e.sectors as u64 * 512, 7 * MIB);
        assert_eq!(mbr::signature(&t.writes[0].1), 0x1234);
    }

    #[test]
    fn retable_rejects_invalid_sector_sizes() {
        let head = mbr::build(1, &[], None);
        for ss in [0, 1, 511, 513, u32::MAX] {
            let t = retable(&head, ss, MIB, 2 * MIB, true, true);
            assert!(t.writes.is_empty());
            assert!(t.extended.is_none());
        }
    }

    #[test]
    fn retable_hostile_gpt_range_does_not_overflow() {
        let g = Gpt {
            disk_id: Uuid::nil(),
            sector_size: 512,
            entry_count: DEFAULT_ENTRY_COUNT,
            entries: vec![GptEntry {
                type_id: gpt::BASIC_DATA,
                id: Uuid::nil(),
                first_lba: 2048,
                last_lba: u64::MAX,
                attributes: 0,
                name: String::new(),
            }],
        };
        let head = g.build(4 * MIB, Some(2048), None).primary;
        let t = retable(&head, 512, 4 * MIB, 8 * MIB, true, true);
        assert!(t.extended.is_none());
    }

    #[test]
    fn patched_overlays_reads() {
        let inner: Arc<dyn BlockDevice> = Arc::new(MemDevice::from_vec(vec![1u8; 4096], 512));
        let p = Patched::new(inner, vec![(10, vec![9, 9, 9])]);
        let mut buf = [0u8; 8];
        p.read_at(8, &mut buf).unwrap();
        assert_eq!(buf, [1, 1, 9, 9, 9, 1, 1, 1]);
        assert!(p.write_at(0, &[0]).is_err());
    }

    #[test]
    fn patched_merges_overlaps_in_order() {
        let inner: Arc<dyn BlockDevice> = Arc::new(MemDevice::from_vec(vec![0u8; 64], 512));
        let patches =
            vec![(20, vec![5; 4]), (2, vec![1; 4]), (4, vec![2; 4]), (0, vec![3; 1]), (30, vec![]), (7, vec![4; 14])];
        let p = Patched::new(inner, patches);
        assert_eq!(p.patches.len(), 2);
        let mut buf = [0u8; 26];
        p.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [3, 0, 1, 1, 2, 2, 2, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 0, 0]);
        let mut buf = [0u8; 3];
        p.read_at(19, &mut buf).unwrap();
        assert_eq!(buf, [4, 4, 5]);
    }
}
