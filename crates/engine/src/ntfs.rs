//! Opening up NTFS permissions for mounted images. Every security descriptor in a volume's
//! `$Secure:$SDS` stream is replaced, as byte patches over the exposed disk (the image is not
//! changed), by one that gives Everyone full control. Files and folders reference descriptors
//! by security ID, so this opens every file of the volume at once.
//!
//! Not covered: EFS-encrypted file contents, and descriptors stored per file in a
//! `$SECURITY_DESCRIPTOR` attribute (only volumes from Windows NT 4 and earlier use those).

use std::io;

use blockdev::{gpt, mbr, BlockDevice};

use crate::fs;

/// Byte offset and replacement data.
pub type Patch = (u64, Vec<u8>);

/// `$SDS` alternates 256 KiB blocks of descriptors with a mirror copy of each block.
const SDS_BLOCK: u64 = 256 * 1024;
const SDS_HEADER: usize = 20;
const SECURE_RECORD: u64 = 9;
const ATTR_LIST: u32 = 0x20;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
/// The update sequence protects the last two bytes of every 512-byte stride of a record.
const FIXUP_STRIDE: usize = 512;
/// Extended partition chains longer than this are treated as corrupt.
const MAX_LOGICAL: usize = 128;

/// Patches that give everyone full access to every NTFS volume on `disk` (a whole disk, or a
/// volume without a partition table), and a warning for each NTFS volume left unchanged.
pub fn open_permissions(disk: &dyn BlockDevice) -> (Vec<Patch>, Vec<String>) {
    let mut patches = Vec::new();
    let mut warnings = Vec::new();
    let offsets = match volume_offsets(disk) {
        Ok(o) => o,
        Err(e) => return (patches, vec![format!("reading the partition table to open permissions: {e}")]),
    };
    for base in offsets {
        match volume_patches(disk, base) {
            Ok(Some(p)) => patches.extend(p),
            Ok(None) => {}
            Err(e) => warnings.push(format!("file permissions of the NTFS volume at offset {base} are unchanged: {e}")),
        }
    }
    (patches, warnings)
}

/// Start offsets of the partitions on `disk` (GPT, MBR including logical partitions, or a
/// single volume at offset 0).
fn volume_offsets(disk: &dyn BlockDevice) -> io::Result<Vec<u64>> {
    let ss = u64::from(disk.block_size());
    let mut head = vec![0u8; disk.size().min(1024 * 1024) as usize];
    disk.read_at(0, &mut head)?;
    if let Some((_, entries)) = gpt::parse_primary(&head, ss as u32) {
        return Ok(entries.iter().map(|e| e.first_lba * ss).collect());
    }
    if fs::detect(&head).is_some() {
        return Ok(vec![0]);
    }
    if !mbr::has_boot_signature(&head) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e in mbr::entries(&head[..512]).into_iter().flatten() {
        if !mbr::is_extended(e.partition_type) {
            out.push(u64::from(e.first_lba) * ss);
            continue;
        }
        // Each extended boot record holds one logical partition (relative to the record) and a
        // link to the next record (relative to the extended partition).
        let base = u64::from(e.first_lba);
        let mut ebr = base;
        let mut sector = vec![0u8; 512];
        for _ in 0..MAX_LOGICAL {
            if (ebr + 1) * ss > disk.size() {
                break;
            }
            disk.read_at(ebr * ss, &mut sector)?;
            if !mbr::has_boot_signature(&sector) {
                break;
            }
            let [logical, next, ..] = mbr::entries(&sector);
            if let Some(l) = logical {
                out.push((ebr + u64::from(l.first_lba)) * ss);
            }
            match next {
                Some(n) if mbr::is_extended(n.partition_type) && n.first_lba != 0 => {
                    ebr = base + u64::from(n.first_lba)
                }
                _ => break,
            }
        }
    }
    Ok(out)
}

/// A contiguous piece of a non-resident attribute; `lcn` is `None` for sparse runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    vcn: u64,
    lcn: Option<u64>,
    len: u64,
}

struct Volume<'a> {
    disk: &'a dyn BlockDevice,
    base: u64,
    cluster: u64,
    record: u64,
    mft: Vec<Run>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u16_at(b: &[u8], at: usize) -> io::Result<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| invalid("truncated record"))
}

fn u32_at(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| invalid("truncated record"))
}

fn u64_at(b: &[u8], at: usize) -> io::Result<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| invalid("truncated record"))
}

/// The patches for the volume at `base`, or `None` if it is not NTFS.
fn volume_patches(disk: &dyn BlockDevice, base: u64) -> io::Result<Option<Vec<Patch>>> {
    if base + 512 > disk.size() {
        return Ok(None);
    }
    let mut boot = [0u8; 512];
    disk.read_at(base, &mut boot)?;
    if fs::detect(&boot) != Some("NTFS") {
        return Ok(None);
    }
    let mut vol = Volume::open(disk, base, &boot)?;
    let mft_record = vol.read_record(0)?;
    if let Some(runs) = vol.data_runs(&mft_record, "")? {
        vol.mft = runs;
    }
    let (runs, size) = vol.sds()?;
    vol.sds_patches(&runs, size).map(Some)
}

impl<'a> Volume<'a> {
    fn open(disk: &'a dyn BlockDevice, base: u64, boot: &[u8]) -> io::Result<Self> {
        let bps = u64::from(u16_at(boot, 0x0B)?);
        let spc = match boot[0x0D] {
            0 => return Err(invalid("zero sectors per cluster")),
            raw @ 1..=0x80 => u64::from(raw),
            raw => 1u64 << (256 - u32::from(raw)).min(31),
        };
        if !bps.is_power_of_two() || !(256..=4096).contains(&bps) {
            return Err(invalid(format!("unsupported sector size {bps}")));
        }
        let cluster = bps * spc;
        let record = match boot[0x40] as i8 {
            n @ 1.. => u64::from(n as u8) * cluster,
            n @ -31..=-1 => 1u64 << -i32::from(n),
            _ => return Err(invalid("invalid MFT record size")),
        };
        if !record.is_power_of_two() || !(512..=65536).contains(&record) {
            return Err(invalid(format!("unsupported MFT record size {record}")));
        }
        let mft_lcn = u64_at(boot, 0x30)?;
        // Until $MFT's own run list is read: its first records are always contiguous.
        let mft = vec![Run { vcn: 0, lcn: Some(mft_lcn), len: 16 * record.div_ceil(cluster) }];
        Ok(Self { disk, base, cluster, record, mft })
    }

    /// Device extents `(offset, length)` holding bytes `offset..offset + len` of a stream.
    fn map(&self, runs: &[Run], offset: u64, len: u64) -> io::Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let (mut at, end) = (offset, offset + len);
        while at < end {
            let vcn = at / self.cluster;
            let run = runs
                .iter()
                .find(|r| r.vcn <= vcn && vcn < r.vcn + r.len)
                .ok_or_else(|| invalid("stream offset outside its run list"))?;
            let lcn = run.lcn.ok_or_else(|| invalid("sparse run in a metadata stream"))?;
            let run_end = (run.vcn + run.len) * self.cluster;
            let piece = end.min(run_end) - at;
            let dev = self.base + (lcn + vcn - run.vcn) * self.cluster + at % self.cluster;
            if dev + piece > self.disk.size() {
                return Err(invalid("run beyond the end of the disk"));
            }
            out.push((dev, piece));
            at += piece;
        }
        Ok(out)
    }

    fn read_stream(&self, runs: &[Run], offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        for (dev, len) in self.map(runs, offset, buf.len() as u64)? {
            self.disk.read_at(dev, &mut buf[done..done + len as usize])?;
            done += len as usize;
        }
        Ok(())
    }

    fn read_record(&self, number: u64) -> io::Result<Vec<u8>> {
        let mut rec = vec![0u8; self.record as usize];
        self.read_stream(&self.mft, number * self.record, &mut rec)?;
        apply_fixups(&mut rec)?;
        Ok(rec)
    }

    /// Run list of the non-resident `$DATA` attribute named `name` in one record (the segment
    /// that record holds), or `None` if absent.
    fn data_runs(&self, rec: &[u8], name: &str) -> io::Result<Option<Vec<Run>>> {
        for a in attributes(rec)? {
            if a.kind == ATTR_DATA && a.name == name {
                if !a.non_resident {
                    return Err(invalid(format!("resident $DATA:{name}")));
                }
                return decode_runs(a.body).map(Some);
            }
        }
        Ok(None)
    }

    /// The run list and size of `$Secure:$SDS`, following an attribute list if it has one.
    fn sds(&self) -> io::Result<(Vec<Run>, u64)> {
        let base = self.read_record(SECURE_RECORD)?;
        let mut runs = Vec::new();
        let mut size = None;
        let mut records = vec![SECURE_RECORD];
        for a in attributes(&base)? {
            if a.kind == ATTR_LIST {
                let list = if a.non_resident {
                    let len = u64_at(a.body, 0x30)?;
                    let mut buf = vec![0u8; usize::try_from(len).map_err(|_| invalid("huge attribute list"))?];
                    self.read_stream(&decode_runs(a.body)?, 0, &mut buf)?;
                    buf
                } else {
                    resident_value(a.body)?.to_vec()
                };
                for (kind, name, record) in attribute_list(&list)? {
                    if kind == ATTR_DATA && name == "$SDS" && !records.contains(&record) {
                        records.push(record);
                    }
                }
            }
        }
        for &n in &records {
            let rec = if n == SECURE_RECORD { base.clone() } else { self.read_record(n)? };
            for a in attributes(&rec)? {
                if a.kind == ATTR_DATA && a.name == "$SDS" {
                    if !a.non_resident {
                        return Err(invalid("resident $SDS"));
                    }
                    if u64_at(a.body, 0x10)? == 0 {
                        size = Some(u64_at(a.body, 0x30)?);
                    }
                    runs.extend(decode_runs(a.body)?);
                }
            }
        }
        let size = size.ok_or_else(|| invalid("$Secure has no $SDS stream"))?;
        Ok((runs, size))
    }

    fn sds_patches(&self, runs: &[Run], size: u64) -> io::Result<Vec<Patch>> {
        let mut patches = Vec::new();
        let mut block = vec![0u8; SDS_BLOCK as usize];
        // Descriptors live in the even blocks; each odd block mirrors the one before it.
        for start in (0..size).step_by(2 * SDS_BLOCK as usize) {
            let len = (size - start).min(SDS_BLOCK) as usize;
            let block = &mut block[..len];
            self.read_stream(runs, start, block)?;
            let mut pos = 0usize;
            while pos + SDS_HEADER <= len {
                let offset = u64_at(block, pos + 8)?;
                let entry = u32_at(block, pos + 16)? as usize;
                if entry <= SDS_HEADER || offset != start + pos as u64 || pos + entry > len {
                    break;
                }
                let at = pos + SDS_HEADER;
                // Only replace what is recognizably a self-relative descriptor.
                if block[at] == 1 && u16_at(block, at + 2)? & SE_SELF_RELATIVE != 0 {
                    if let Some(sd) = open_descriptor(entry - SDS_HEADER) {
                        for copy in [start + at as u64, start + SDS_BLOCK + at as u64] {
                            if copy + sd.len() as u64 > size {
                                continue;
                            }
                            let mut done = 0usize;
                            for (dev, piece) in self.map(runs, copy, sd.len() as u64)? {
                                patches.push((dev, sd[done..done + piece as usize].to_vec()));
                                done += piece as usize;
                            }
                        }
                    }
                }
                pos += entry.next_multiple_of(16);
            }
        }
        Ok(patches)
    }
}

/// Undoes the update sequence protection of an MFT record.
fn apply_fixups(rec: &mut [u8]) -> io::Result<()> {
    if &rec[..4] != b"FILE" {
        return Err(invalid("not an MFT record"));
    }
    let usa = u16_at(rec, 4)? as usize;
    let count = u16_at(rec, 6)? as usize;
    if count == 0 || usa + 2 * count > rec.len() || (count - 1) * FIXUP_STRIDE > rec.len() {
        return Err(invalid("invalid update sequence"));
    }
    let usn = [rec[usa], rec[usa + 1]];
    for i in 1..count {
        let end = i * FIXUP_STRIDE;
        if rec[end - 2..end] != usn {
            return Err(invalid("torn MFT record"));
        }
        let (a, b) = (rec[usa + 2 * i], rec[usa + 2 * i + 1]);
        rec[end - 2] = a;
        rec[end - 1] = b;
    }
    Ok(())
}

struct Attribute<'a> {
    kind: u32,
    non_resident: bool,
    name: String,
    body: &'a [u8],
}

fn attributes(rec: &[u8]) -> io::Result<Vec<Attribute<'_>>> {
    if u16_at(rec, 0x16)? & 1 == 0 {
        return Err(invalid("MFT record not in use"));
    }
    let mut out = Vec::new();
    let mut at = u16_at(rec, 0x14)? as usize;
    loop {
        let kind = u32_at(rec, at)?;
        if kind == ATTR_END {
            return Ok(out);
        }
        let len = u32_at(rec, at + 4)? as usize;
        if len < 16 || at + len > rec.len() {
            return Err(invalid("invalid attribute length"));
        }
        let body = &rec[at..at + len];
        let name_len = body[9] as usize;
        let name_at = u16_at(body, 10)? as usize;
        let name = body.get(name_at..name_at + 2 * name_len).ok_or_else(|| invalid("invalid attribute name"))?;
        out.push(Attribute { kind, non_resident: body[8] != 0, name: utf16(name), body });
        at += len;
    }
}

fn utf16(b: &[u8]) -> String {
    String::from_utf16_lossy(&b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect::<Vec<_>>())
}

fn resident_value(body: &[u8]) -> io::Result<&[u8]> {
    let len = u32_at(body, 0x10)? as usize;
    let at = u16_at(body, 0x14)? as usize;
    body.get(at..at + len).ok_or_else(|| invalid("invalid resident value"))
}

/// `(type, name, MFT record number)` of each attribute list entry.
fn attribute_list(list: &[u8]) -> io::Result<Vec<(u32, String, u64)>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 0x1A <= list.len() {
        let kind = u32_at(list, at)?;
        let len = u16_at(list, at + 4)? as usize;
        if len < 0x1A || at + len > list.len() {
            break;
        }
        let name_len = list[at + 6] as usize;
        let name_at = at + list[at + 7] as usize;
        let name = list.get(name_at..name_at + 2 * name_len).ok_or_else(|| invalid("invalid attribute list"))?;
        let name = utf16(name);
        out.push((kind, name, u64_at(list, at + 0x10)? & 0xFFFF_FFFF_FFFF));
        at += len;
    }
    Ok(out)
}

/// Decodes the run list of a non-resident attribute header.
fn decode_runs(body: &[u8]) -> io::Result<Vec<Run>> {
    let mut vcn = u64_at(body, 0x10)?;
    let mut at = u16_at(body, 0x20)? as usize;
    let mut lcn = 0i64;
    let mut out = Vec::new();
    loop {
        let header = *body.get(at).ok_or_else(|| invalid("unterminated run list"))?;
        if header == 0 {
            return Ok(out);
        }
        let (len_size, off_size) = ((header & 0x0F) as usize, (header >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(invalid("invalid run header"));
        }
        let field = |from: usize, size: usize| -> io::Result<i64> {
            let b = body.get(from..from + size).ok_or_else(|| invalid("truncated run list"))?;
            let mut v = [0u8; 8];
            v[..size].copy_from_slice(b);
            if size > 0 && size < 8 && b[size - 1] & 0x80 != 0 {
                v[size..].fill(0xFF);
            }
            Ok(i64::from_le_bytes(v))
        };
        let len = field(at + 1, len_size)?;
        if len <= 0 {
            return Err(invalid("invalid run length"));
        }
        let run_lcn = if off_size == 0 {
            None
        } else {
            lcn = lcn
                .checked_add(field(at + 1 + len_size, off_size)?)
                .filter(|l| *l >= 0)
                .ok_or_else(|| invalid("invalid run offset"))?;
            Some(lcn as u64)
        };
        out.push(Run { vcn, lcn: run_lcn, len: len as u64 });
        vcn += len as u64;
        at += 1 + len_size + off_size;
    }
}

const SE_DACL_PRESENT: u16 = 0x0004;
const SE_SELF_RELATIVE: u16 = 0x8000;
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// Object and container inherit, so new files and folders get the same access.
const INHERIT: u8 = 0x03;
const EVERYONE: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
const ADMINISTRATORS: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 2, 0, 0];
const SYSTEM: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];

/// A self-relative descriptor of exactly `len` bytes whose DACL grants Everyone full control,
/// owned by Administrators when there is room. Its ACL absorbs the spare bytes, so the
/// descriptor fills the space of the one it replaces. `None` if even the smallest form does
/// not fit.
fn open_descriptor(len: usize) -> Option<Vec<u8>> {
    const ACE: usize = 8 + EVERYONE.len();
    let owned = 20 + ADMINISTRATORS.len() + SYSTEM.len();
    let dacl_at = if len >= owned + 8 + ACE { owned } else { 20 };
    if len < dacl_at + 8 + ACE {
        return None;
    }
    let mut sd = vec![0u8; len];
    sd[0] = 1;
    sd[2..4].copy_from_slice(&(SE_SELF_RELATIVE | SE_DACL_PRESENT).to_le_bytes());
    if dacl_at == owned {
        sd[4..8].copy_from_slice(&20u32.to_le_bytes());
        sd[8..12].copy_from_slice(&(20 + ADMINISTRATORS.len() as u32).to_le_bytes());
        sd[20..36].copy_from_slice(&ADMINISTRATORS);
        sd[36..48].copy_from_slice(&SYSTEM);
    }
    sd[16..20].copy_from_slice(&(dacl_at as u32).to_le_bytes());
    let acl_size = ((len - dacl_at) & !3).min(0xFFFC) as u16;
    let acl = &mut sd[dacl_at..];
    acl[0] = 2;
    acl[2..4].copy_from_slice(&acl_size.to_le_bytes());
    acl[4..6].copy_from_slice(&1u16.to_le_bytes());
    let ace = &mut acl[8..8 + ACE];
    ace[0] = 0; // ACCESS_ALLOWED_ACE_TYPE
    ace[1] = INHERIT;
    ace[2..4].copy_from_slice(&(ACE as u16).to_le_bytes());
    ace[4..8].copy_from_slice(&FILE_ALL_ACCESS.to_le_bytes());
    ace[8..].copy_from_slice(&EVERYONE);
    Some(sd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockdev::MemDevice;

    const CLUSTER: u64 = 4096;
    const RECORD: usize = 1024;
    const MFT_LCN: u64 = 4;
    const SDS_LCN: u64 = 64;

    /// Writes an MFT record with the given attributes (already encoded) and its fixups.
    fn record(attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
        let mut at = 0x38;
        for a in attrs {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        let usn = [0x07, 0x00];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 1..3 {
            let end = i * FIXUP_STRIDE;
            let saved = [r[end - 2], r[end - 1]];
            r[0x30 + 2 * i..0x32 + 2 * i].copy_from_slice(&saved);
            r[end - 2..end].copy_from_slice(&usn);
        }
        r
    }

    /// A non-resident $DATA attribute with one run.
    fn data(name: &str, lcn: u64, clusters: u64, size: u64) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let runs_at = 0x40 + name.len().next_multiple_of(8);
        let mut a = vec![0u8; (runs_at + 16).next_multiple_of(8)];
        a[..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a[8] = 1;
        a[9] = (name.len() / 2) as u8;
        a[10..12].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x18..0x20].copy_from_slice(&(clusters - 1).to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(runs_at as u16).to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x40..0x40 + name.len()].copy_from_slice(&name);
        a[runs_at] = 0x22;
        a[runs_at + 1..runs_at + 3].copy_from_slice(&(clusters as u16).to_le_bytes());
        a[runs_at + 3..runs_at + 5].copy_from_slice(&(lcn as u16).to_le_bytes());
        a
    }

    /// A restrictive descriptor: owner SYSTEM, DACL granting only SYSTEM.
    fn restrictive() -> Vec<u8> {
        let mut sd = vec![0u8; 20 + 12 + 8 + 20];
        sd[0] = 1;
        sd[2..4].copy_from_slice(&(SE_SELF_RELATIVE | SE_DACL_PRESENT).to_le_bytes());
        sd[4..8].copy_from_slice(&20u32.to_le_bytes());
        sd[16..20].copy_from_slice(&32u32.to_le_bytes());
        sd[20..32].copy_from_slice(&SYSTEM);
        sd[32] = 2;
        sd[34..36].copy_from_slice(&28u16.to_le_bytes());
        sd[36..38].copy_from_slice(&1u16.to_le_bytes());
        sd[40..42].copy_from_slice(&[0, 0]);
        sd[42..44].copy_from_slice(&20u16.to_le_bytes());
        sd[44..48].copy_from_slice(&FILE_ALL_ACCESS.to_le_bytes());
        sd[48..60].copy_from_slice(&SYSTEM);
        sd
    }

    /// An NTFS volume at `base` of a 4 MiB disk with two descriptors in $SDS (and the mirror).
    fn volume(dev: &MemDevice, base: u64, sds: &[Vec<u8>]) -> Vec<(u64, usize)> {
        let mut boot = crate::fs::fake_ntfs_boot(4096, 8);
        boot[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
        boot[0x40] = (-10i8) as u8;
        dev.write_at(base, &boot).unwrap();
        let mft = base + MFT_LCN * CLUSTER;
        dev.write_at(mft, &record(&[data("", MFT_LCN, 4, 16 * RECORD as u64)])).unwrap();
        let size = 2 * SDS_BLOCK;
        dev.write_at(mft + 9 * RECORD as u64, &record(&[data("$SDS", SDS_LCN, size / CLUSTER, size)])).unwrap();
        let mut stream = vec![0u8; SDS_BLOCK as usize];
        let mut pos = 0usize;
        let mut entries = Vec::new();
        for (i, sd) in sds.iter().enumerate() {
            let len = SDS_HEADER + sd.len();
            stream[pos..pos + 4].copy_from_slice(&0xABCDu32.to_le_bytes());
            stream[pos + 4..pos + 8].copy_from_slice(&(0x100 + i as u32).to_le_bytes());
            stream[pos + 8..pos + 16].copy_from_slice(&(pos as u64).to_le_bytes());
            stream[pos + 16..pos + 20].copy_from_slice(&(len as u32).to_le_bytes());
            stream[pos + 20..pos + len].copy_from_slice(sd);
            entries.push((pos as u64 + SDS_HEADER as u64, sd.len()));
            pos += len.next_multiple_of(16);
        }
        let sds_at = base + SDS_LCN * CLUSTER;
        dev.write_at(sds_at, &stream).unwrap();
        dev.write_at(sds_at + SDS_BLOCK, &stream).unwrap();
        entries.into_iter().map(|(at, len)| (sds_at + at, len)).collect()
    }

    fn read(dev: &MemDevice, patches: &[Patch], at: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        dev.read_at(at, &mut buf).unwrap();
        for (p, data) in patches {
            for (i, b) in data.iter().enumerate() {
                let o = p + i as u64;
                if o >= at && o < at + len as u64 {
                    buf[(o - at) as usize] = *b;
                }
            }
        }
        buf
    }

    #[test]
    fn opens_every_descriptor_and_its_mirror() {
        let dev = MemDevice::new(4 * 1024 * 1024, 512);
        let mut big = restrictive();
        big.extend_from_slice(&[0u8; 40]); // room for an owner
        let entries = volume(&dev, 0, &[restrictive(), big]);
        let (patches, warnings) = open_permissions(&dev);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(patches.len(), 4);
        for (at, len) in entries {
            for copy in [at, at + SDS_BLOCK] {
                let sd = read(&dev, &patches, copy, len);
                assert_eq!(sd, open_descriptor(len).unwrap());
                let dacl = u32::from_le_bytes(sd[16..20].try_into().unwrap()) as usize;
                assert_eq!(&sd[dacl + 16..dacl + 28], &EVERYONE);
                assert_eq!(u32::from_le_bytes(sd[dacl + 12..dacl + 16].try_into().unwrap()), FILE_ALL_ACCESS);
                let owner = u32::from_le_bytes(sd[4..8].try_into().unwrap()) as usize;
                if len >= 76 {
                    assert_eq!(&sd[owner..owner + 16], &ADMINISTRATORS);
                } else {
                    assert_eq!(owner, 0);
                }
            }
        }
        // Headers (hash, ID, offset, length) are untouched.
        assert!(patches.iter().all(|(p, _)| (*p - SDS_LCN * CLUSTER) % 16 == 4));
    }

    #[test]
    fn finds_volumes_in_gpt_and_mbr_tables() {
        let dev = MemDevice::new(4 * 1024 * 1024, 512);
        volume(&dev, 1024 * 1024, &[restrictive()]);
        let primary = mbr::MbrEntry { bootable: false, partition_type: 7, first_lba: 2048, sectors: 4096 };
        dev.write_at(0, &mbr::build(1, &[primary], None)).unwrap();
        let (patches, warnings) = open_permissions(&dev);
        assert!(warnings.is_empty() && patches.len() == 2, "{warnings:?}");

        // The same volume as the only logical partition of an extended partition whose boot
        // record sits one sector before it.
        let extended = mbr::MbrEntry { bootable: false, partition_type: 0x0F, first_lba: 2047, sectors: 4097 };
        dev.write_at(0, &mbr::build(1, &[extended], None)).unwrap();
        let logical = mbr::MbrEntry { bootable: false, partition_type: 7, first_lba: 1, sectors: 4096 };
        dev.write_at(2047 * 512, &mbr::build(0, &[logical], None)).unwrap();
        assert_eq!(volume_offsets(&dev).unwrap(), vec![1024 * 1024]);
    }

    #[test]
    fn leaves_other_file_systems_and_reports_broken_ntfs() {
        let dev = MemDevice::new(4 * 1024 * 1024, 512);
        assert_eq!(open_permissions(&dev), (vec![], vec![]));
        volume(&dev, 0, &[restrictive()]);
        dev.write_at(MFT_LCN * CLUSTER + 9 * RECORD as u64, b"BAAD").unwrap();
        let (patches, warnings) = open_permissions(&dev);
        assert!(patches.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn descriptor_sizes() {
        assert!(open_descriptor(47).is_none());
        let small = open_descriptor(48).unwrap();
        assert_eq!(u32::from_le_bytes(small[4..8].try_into().unwrap()), 0);
        let padded = open_descriptor(4096 + 2).unwrap();
        assert_eq!(padded.len(), 4098);
        let acl_size = u16::from_le_bytes(padded[50..52].try_into().unwrap()) as usize;
        assert_eq!(acl_size, (4098 - 48) & !3);
    }

    #[test]
    fn signed_run_offsets() {
        let mut a = vec![0u8; 0x40];
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        // 4 clusters at LCN 0x100, 2 clusters 0x10 before that, then 3 sparse clusters.
        a.extend_from_slice(&[0x21, 4, 0x00, 0x01, 0x11, 2, 0xF0, 0x01, 3, 0]);
        let runs = decode_runs(&a).unwrap();
        assert_eq!(
            runs,
            vec![
                Run { vcn: 0, lcn: Some(0x100), len: 4 },
                Run { vcn: 4, lcn: Some(0xF0), len: 2 },
                Run { vcn: 6, lcn: None, len: 3 },
            ]
        );
    }
}
