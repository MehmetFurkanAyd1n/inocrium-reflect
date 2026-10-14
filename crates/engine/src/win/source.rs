//! Backup sources: physical disks read through a VSS snapshot of their volumes.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use blockdev::{BlockDevice, Slice};
use irimg::{CaptureMode, DiskManifest, GptPartitionInfo, MbrPartitionInfo, PartitionManifest, PartitionStyle};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vss::{Snapshot, SnapshotSet};
use winio::{DiskInfo, LayoutStyle, PartitionInfo, PartitionKind, RawDisk, Volume};

use crate::backup::{SourceDisk, SourcePartition};
use crate::extents::Extents;
use crate::{fs, Error, Result};

/// Files whose contents are never worth imaging (Windows recreates them).
const EXCLUDED_FILES: [&str; 3] = ["pagefile.sys", "hiberfil.sys", "swapfile.sys"];

#[derive(Debug, Clone, Default)]
pub struct PrepareOptions {
    /// Capture every sector of every partition and the unpartitioned space.
    pub forensic: bool,
    /// Create a persistent snapshot (survives the process, needed to resume a backup).
    pub persistent_snapshot: bool,
    /// Reuse this persistent snapshot set (resuming an interrupted backup).
    pub resume_snapshot: Option<Uuid>,
}

/// A disk to image and, optionally, which of its partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSelection {
    pub disk: u32,
    /// Partition numbers; `None` selects all of them.
    #[serde(default)]
    pub partitions: Option<Vec<u32>>,
}

/// The snapshot set the sources read from.
pub enum Shadow {
    Owned {
        set: SnapshotSet,
        persistent: bool,
    },
    /// A persistent set created by an earlier process.
    Existing {
        id: Uuid,
        snapshots: Vec<Snapshot>,
    },
}

impl Shadow {
    pub fn set_id(&self) -> Uuid {
        match self {
            Shadow::Owned { set, .. } => set.id(),
            Shadow::Existing { id, .. } => *id,
        }
    }

    pub fn is_persistent(&self) -> bool {
        match self {
            Shadow::Owned { persistent, .. } => *persistent,
            Shadow::Existing { .. } => true,
        }
    }

    /// The shadow device of `volume` (a GUID path).
    pub fn device_for(&self, volume: &str) -> Option<&str> {
        match self {
            Shadow::Owned { set, .. } => set.device_for(volume),
            Shadow::Existing { snapshots, .. } => snapshots
                .iter()
                .find(|s| s.original_volume.trim_end_matches('\\').eq_ignore_ascii_case(volume.trim_end_matches('\\')))
                .map(|s| s.device_path.as_str()),
        }
    }

    /// Ends the backup. Persistent snapshots are kept when `keep` is set (to resume later)
    /// and deleted otherwise; non-persistent ones always go away.
    pub fn release(self, success: bool, keep: bool) -> Result<()> {
        match self {
            Shadow::Owned { set, persistent } => {
                let id = set.id();
                let completed = set.complete(success);
                if persistent && !keep {
                    vss::delete_snapshot_set(id)?;
                }
                Ok(completed?)
            }
            Shadow::Existing { id, .. } => {
                if !keep {
                    vss::delete_snapshot_set(id)?;
                }
                Ok(())
            }
        }
    }
}

pub struct PreparedSources {
    pub disks: Vec<SourceDisk>,
    pub shadow: Option<Shadow>,
    /// Things the user should know about (e.g. a partition captured sector by sector).
    pub warnings: Vec<String>,
}

impl PreparedSources {
    /// Closes the source devices and releases the snapshot (see [`Shadow::release`]).
    pub fn release(mut self, success: bool, keep: bool) -> Result<()> {
        self.disks.clear();
        match self.shadow.take() {
            Some(s) => s.release(success, keep),
            None => Ok(()),
        }
    }
}

/// Opens the selected disks for [`crate::backup`]: snapshots their NTFS / ReFS volumes and
/// computes the allocated ranges of each partition (minus pagefile, hiberfil and swapfile).
/// Partitions that cannot be snapshotted or whose file system is unknown are captured sector
/// by sector.
pub fn prepare_backup(selection: &[DiskSelection], opts: &PrepareOptions) -> Result<PreparedSources> {
    if selection.is_empty() {
        return Err(Error::invalid("no disks selected"));
    }
    let _ = winio::enable_privileges(&["SeBackupPrivilege", "SeManageVolumePrivilege"]);

    let mut disks = Vec::new();
    for (i, sel) in selection.iter().enumerate() {
        if selection[..i].iter().any(|s| s.disk == sel.disk) {
            return Err(Error::invalid(format!("disk {} is selected twice", sel.disk)));
        }
        let info = winio::disk_info(sel.disk)?;
        let parts: Vec<PartitionInfo> = match &sel.partitions {
            None => info.layout.partitions.clone(),
            Some(numbers) => {
                if numbers.is_empty() {
                    return Err(Error::invalid(format!("no partitions of disk {} selected", sel.disk)));
                }
                let mut v = Vec::new();
                for &n in numbers {
                    let p = info
                        .layout
                        .partitions
                        .iter()
                        .find(|p| p.number == n)
                        .ok_or_else(|| Error::invalid(format!("disk {} has no partition {n}", sel.disk)))?;
                    if !v.iter().any(|q: &PartitionInfo| q.number == n) {
                        v.push(p.clone());
                    }
                }
                v
            }
        };
        disks.push((i as u32, info, parts));
    }

    // Snapshot every selected volume with a file system VSS understands.
    let mut volumes: Vec<String> = Vec::new();
    for (_, _, parts) in &disks {
        for p in parts {
            if let Some(v) = p.volume.as_ref().filter(|v| fs::smart_capable(&v.filesystem)) {
                if !volumes.iter().any(|x| x.eq_ignore_ascii_case(&v.guid_path)) {
                    volumes.push(v.guid_path.clone());
                }
            }
        }
    }
    let shadow = match opts.resume_snapshot {
        Some(id) => {
            let snapshots = vss::find_snapshot_set(id)?.ok_or_else(|| {
                Error::invalid("the snapshot of the interrupted backup no longer exists; start a new backup")
            })?;
            let shadow = Shadow::Existing { id, snapshots };
            if let Some(v) = volumes.iter().find(|v| shadow.device_for(v).is_none()) {
                return Err(Error::invalid(format!(
                    "the snapshot of the interrupted backup does not cover volume {v}; start a new backup"
                )));
            }
            Some(shadow)
        }
        None if volumes.is_empty() => None,
        None => Some(Shadow::Owned {
            set: SnapshotSet::create(&volumes, opts.persistent_snapshot)?,
            persistent: opts.persistent_snapshot,
        }),
    };

    let mut warnings = Vec::new();
    let mut out = Vec::new();
    for (index, info, parts) in disks {
        let raw = Arc::new(RawDisk::open(info.number, false)?);
        let manifest = manifest(index, &info);
        let mut partitions = Vec::new();
        for p in &parts {
            let (device, used) = partition_source(&raw, &info, p, shadow.as_ref(), opts.forensic, &mut warnings)?;
            partitions.push(SourcePartition { number: p.number, device, used });
        }
        let forensic_gaps = opts.forensic || matches!(info.layout.style, LayoutStyle::Raw);
        out.push(SourceDisk { device: raw, manifest, partitions, forensic_gaps });
    }
    Ok(PreparedSources { disks: out, shadow, warnings })
}

fn partition_source(
    raw: &Arc<RawDisk>,
    disk: &DiskInfo,
    p: &PartitionInfo,
    shadow: Option<&Shadow>,
    forensic: bool,
    warnings: &mut Vec<String>,
) -> Result<(Arc<dyn BlockDevice>, Option<Extents>)> {
    let direct: Arc<dyn BlockDevice> = Arc::new(Slice::new(raw.clone(), p.offset, p.length)?);
    let Some(v) = &p.volume else { return Ok((direct, None)) };
    if !fs::smart_capable(&v.filesystem) {
        return Ok((direct, None));
    }
    let Some(device) = shadow.and_then(|s| s.device_for(&v.guid_path)) else {
        // A volume that should have been snapshotted but was not: reading it live would be
        // inconsistent.
        return Err(Error::invalid(format!(
            "disk {} partition {}: no snapshot of volume {}",
            disk.number, p.number, v.guid_path
        )));
    };
    let snap = Volume::open(device, false)?;
    let head_len = snap.length()?.min(p.length);
    let used = if forensic {
        None
    } else {
        match used_extents(&snap, device, &v.guid_path, p.length) {
            Ok(e) => Some(e),
            Err(e) => {
                warnings.push(format!(
                    "disk {} partition {}: allocation map unavailable ({e}); captured sector by sector",
                    disk.number, p.number
                ));
                None
            }
        }
    };
    let tail = Slice::new(raw.clone(), p.offset, p.length)?;
    Ok((Arc::new(Padded { head: snap, head_len, tail, len: p.length }), used))
}

/// The allocated bytes of a snapshotted volume, relative to the partition start.
fn used_extents(snap: &Volume, device: &str, live: &str, len: u64) -> Result<Extents> {
    let bitmap = snap.bitmap()?;
    let cs = bitmap.cluster_size as u64;
    let base = bitmap.cluster0_offset;
    let mut ranges: Vec<std::ops::Range<u64>> = Vec::new();
    if base > 0 {
        ranges.push(0..base);
    }
    let mut run: Option<u64> = None;
    for c in 0..bitmap.total_clusters {
        let set = bitmap.bits[(c / 8) as usize] & (1 << (c % 8)) != 0;
        match (set, run) {
            (true, None) => run = Some(c),
            (false, Some(s)) => {
                ranges.push(base + s * cs..base + c * cs);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(s) = run {
        ranges.push(base + s * cs..base + bitmap.total_clusters * cs);
    }
    let used = Extents::from_ranges(ranges);

    let mut excluded = Extents::new();
    for name in EXCLUDED_FILES {
        let candidates = [PathBuf::from(format!(r"{device}\{name}")), PathBuf::from(format!(r"{live}{name}"))];
        let Some(extents) = candidates.iter().find_map(|p| winio::file_extents(p).ok()) else { continue };
        for e in extents.iter().filter(|e| e.lcn != u64::MAX) {
            excluded.add(base + e.lcn * cs..base + (e.lcn + e.clusters) * cs);
        }
    }
    Ok(used.subtract(&excluded).clip(len))
}

fn manifest(index: u32, info: &DiskInfo) -> DiskManifest {
    let ss = info.sector_size as u64;
    let layout = match &info.layout.style {
        LayoutStyle::Mbr { signature } => PartitionStyle::Mbr { signature: *signature },
        LayoutStyle::Gpt { disk_id, starting_usable_offset, usable_length, max_partition_count } => {
            PartitionStyle::Gpt {
                disk_id: *disk_id,
                first_usable_lba: starting_usable_offset / ss,
                last_usable_lba: (starting_usable_offset + usable_length) / ss - 1,
                max_partition_count: *max_partition_count,
            }
        }
        LayoutStyle::Raw => PartitionStyle::Raw,
    };
    let partitions = info
        .layout
        .partitions
        .iter()
        .map(|p| {
            let (gpt, mbr, esp_or_active) = match &p.kind {
                PartitionKind::Gpt { type_id, id, name, attributes } => (
                    Some(GptPartitionInfo { type_id: *type_id, id: *id, name: name.clone(), attributes: *attributes }),
                    None,
                    *type_id == Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b),
                ),
                PartitionKind::Mbr { partition_type, bootable, logical } => (
                    None,
                    Some(MbrPartitionInfo { partition_type: *partition_type, bootable: *bootable, logical: *logical }),
                    *bootable,
                ),
            };
            let v = p.volume.as_ref();
            PartitionManifest {
                number: p.number,
                stream: 0,
                offset: p.offset,
                length: p.length,
                captured: false,
                capture: CaptureMode::Forensic,
                gpt,
                mbr,
                filesystem: v.map(|v| v.filesystem.clone()).unwrap_or_default(),
                label: v.map(|v| v.label.clone()).unwrap_or_default(),
                drive_letter: v.and_then(|v| v.drive_letters.first().copied()),
                used_bytes: v.filter(|v| v.total_bytes > 0).map(|v| v.total_bytes.saturating_sub(v.free_bytes)),
                stored_bytes: 0,
                is_system: esp_or_active || v.is_some_and(|v| v.is_system),
                is_boot: v.is_some_and(|v| v.is_windows),
            }
        })
        .collect();
    DiskManifest {
        index,
        source_number: info.number,
        model: info.model.clone(),
        serial: info.serial.clone(),
        size: info.size,
        sector_size: info.sector_size,
        physical_sector_size: info.physical_sector_size,
        layout,
        partitions,
        gaps: Vec::new(),
        raw_regions: Vec::new(),
    }
}

/// A partition read through its volume's snapshot. The snapshot device can be a little
/// shorter than the partition (e.g. the NTFS backup boot sector lies past the volume); the
/// rest is read from the disk.
struct Padded {
    head: Volume,
    head_len: u64,
    tail: Slice<Arc<RawDisk>>,
    len: u64,
}

impl BlockDevice for Padded {
    fn size(&self) -> u64 {
        self.len
    }
    fn block_size(&self) -> u32 {
        self.tail.block_size()
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|e| e > self.len) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "read beyond end of partition"));
        }
        let split = (self.head_len.saturating_sub(offset) as usize).min(buf.len());
        let (head, tail) = buf.split_at_mut(split);
        if !head.is_empty() {
            self.head.read_at(offset, head)?;
        }
        if !tail.is_empty() {
            self.tail.read_at(offset + split as u64, tail)?;
        }
        Ok(())
    }
    fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "backup sources are read-only"))
    }
}
