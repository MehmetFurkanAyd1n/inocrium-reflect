//! Which bytes of a source disk outside its partitions an image keeps.

use blockdev::{gpt, mbr, BlockDevice};
use irimg::{DiskManifest, GapManifest, PartitionStyle, RawRegionInfo, RawRegionKind};

use crate::extents::Extents;
use crate::Result;

const MIB: u64 = 1024 * 1024;
/// Upper bound for TRACK0 (boot code, partition table, vendor data before the first partition).
pub const TRACK0_MAX: u64 = 16 * MIB;
/// Stream numbers of forensic gap streams start here.
pub const GAP_STREAM_BASE: u32 = 0x8000_0000;
const MAX_EBRS: usize = 256;

/// Raw regions of disk `d` read from `disk`: TRACK0, the GPT backup area and every EBR.
pub fn raw_regions(disk: &dyn BlockDevice, d: &DiskManifest) -> Result<Vec<RawRegionInfo>> {
    let size = d.size.min(disk.size());
    let ss = d.sector_size as u64;
    let first_partition = d.partitions.iter().map(|p| p.offset).min();
    let mut track0 = match first_partition {
        Some(off) => off.min(TRACK0_MAX),
        None => size.min(MIB),
    };
    let mut out = Vec::new();
    if let PartitionStyle::Gpt { last_usable_lba, max_partition_count, .. } = &d.layout {
        let entries = (*max_partition_count).max(gpt::DEFAULT_ENTRY_COUNT) as u64 * gpt::GPT_ENTRY_SIZE as u64;
        track0 = track0.max((2 + entries.div_ceil(ss)) * ss);
        let backup = (last_usable_lba + 1) * ss;
        if backup < size {
            out.push(RawRegionInfo { offset: backup, length: size - backup, kind: RawRegionKind::GptBackup });
        }
    }
    let track0 = track0.min(size);
    if track0 > 0 {
        out.insert(0, RawRegionInfo { offset: 0, length: track0, kind: RawRegionKind::Track0 });
    }
    if matches!(d.layout, PartitionStyle::Mbr { .. }) {
        for offset in ebr_chain(disk, ss)? {
            if offset >= track0 && offset + ss <= size {
                out.push(RawRegionInfo { offset, length: ss, kind: RawRegionKind::Ebr });
            }
        }
    }
    out.sort_by_key(|r| r.offset);
    Ok(out)
}

/// Byte offsets of the EBRs of an MBR disk's extended partition, in chain order.
fn ebr_chain(disk: &dyn BlockDevice, ss: u64) -> Result<Vec<u64>> {
    let mut sector = vec![0u8; ss.max(512) as usize];
    disk.read_at(0, &mut sector)?;
    if !mbr::has_boot_signature(&sector) {
        return Ok(vec![]);
    }
    let Some(ext) = mbr::entries(&sector).into_iter().flatten().find(|e| mbr::is_extended(e.partition_type)) else {
        return Ok(vec![]);
    };
    let base = ext.first_lba as u64;
    let mut out = Vec::new();
    let mut lba = base;
    while out.len() < MAX_EBRS && lba * ss + ss <= disk.size() {
        disk.read_at(lba * ss, &mut sector)?;
        if !mbr::has_boot_signature(&sector) {
            break;
        }
        out.push(lba * ss);
        // Entry 2 of an EBR links to the next EBR, relative to the extended partition.
        match mbr::entries(&sector)[1] {
            Some(next) if mbr::is_extended(next.partition_type) && next.first_lba != 0 => {
                let n = base + next.first_lba as u64;
                if n <= lba {
                    break;
                }
                lba = n;
            }
            _ => break,
        }
    }
    Ok(out)
}

/// Unpartitioned areas not covered by `raw`, as forensic gap streams.
pub fn gaps(d: &DiskManifest, raw: &[RawRegionInfo]) -> Vec<GapManifest> {
    let covered = Extents::from_ranges(
        d.partitions
            .iter()
            .map(|p| p.offset..p.offset + p.length)
            .chain(raw.iter().map(|r| r.offset..r.offset + r.length)),
    );
    covered
        .gaps(d.size)
        .ranges()
        .iter()
        .enumerate()
        .map(|(i, r)| GapManifest { stream: GAP_STREAM_BASE + i as u32, offset: r.start, length: r.end - r.start })
        .collect()
}
