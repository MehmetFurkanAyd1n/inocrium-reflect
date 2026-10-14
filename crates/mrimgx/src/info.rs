//! Building [`DiskInfo`]/[`PartitionInfo`] from the `$JSON` disk entries and track 0.

use crate::format::{u64_at, DiskJson, PartitionJson};
use crate::image::{DiskInfo, DiskLayout, PartitionInfo};

/// Partition table style as detected from track 0.
#[derive(PartialEq, Eq)]
enum Style {
    Mbr,
    Gpt,
    Raw,
}

fn detect_style(track0: &[u8]) -> Style {
    match track0.get(..512) {
        Some(mbr) if mbr[510..512] == [0x55, 0xAA] => {
            // A protective MBR has a partition of type 0xEE.
            if (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xEE) {
                Style::Gpt
            } else {
                Style::Mbr
            }
        }
        _ => Style::Raw,
    }
}

fn guid(b: &[u8]) -> String {
    let b: [u8; 16] = b.try_into().expect("GUID is 16 bytes");
    uuid::Uuid::from_bytes_le(b).hyphenated().to_string().to_uppercase()
}

/// Primary GPT header and entries as stored in track 0: `(disk GUID, [(first LBA, type GUID)])`.
fn parse_gpt(track0: &[u8], sector: usize) -> Option<(String, Vec<(u64, String)>)> {
    let hdr = track0.get(sector..sector + 92)?;
    if &hdr[..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = u64_at(hdr, 72);
    let count = u32::from_le_bytes(hdr[80..84].try_into().ok()?) as usize;
    let size = u32::from_le_bytes(hdr[84..88].try_into().ok()?) as usize;
    let mut parts = Vec::new();
    if size >= 128 {
        let base = usize::try_from(entries_lba).ok()?.checked_mul(sector)?;
        // Only entries captured in track 0 are visible; count is capped by the buffer anyway.
        for i in 0..count.min(4096) {
            let entry = i.checked_mul(size).and_then(|o| o.checked_add(base));
            let Some(e) = entry.and_then(|o| track0.get(o..)).and_then(|e| e.get(..128)) else { break };
            if e[..16].iter().any(|&b| b != 0) {
                parts.push((u64_at(e, 32), guid(&e[..16])));
            }
        }
    }
    Some((guid(&hdr[56..72]), parts))
}

pub(crate) fn disk_info(dj: &DiskJson, track0: &[u8], layout: &DiskLayout) -> DiskInfo {
    let detected = detect_style(track0);
    let style = match dj.format.to_ascii_lowercase().as_str() {
        "gpt" => Style::Gpt,
        "mbr" => Style::Mbr,
        // Dynamic disks keep an MBR or GPT partition table; anything else is inspected.
        _ => detected,
    };
    let sector = dj.sector_size as usize;
    let gpt = (style == Style::Gpt).then(|| parse_gpt(track0, sector)).flatten();
    let signature_or_guid = if !dj.signature.is_empty() {
        dj.signature.clone()
    } else if let Some((disk_guid, _)) = &gpt {
        disk_guid.clone()
    } else if style == Style::Mbr && track0.len() >= 444 {
        format!("{:08X}", u32::from_le_bytes(track0[440..444].try_into().unwrap()))
    } else {
        String::new()
    };
    let partitions = dj
        .partitions
        .iter()
        .zip(&layout.partitions)
        .enumerate()
        .map(|(index, (pj, pl))| PartitionInfo {
            index,
            number: u32::from(pj.number),
            offset: pl.start,
            length: pl.len,
            filesystem: pj.fs_type.clone(),
            label: pj.label.clone(),
            drive_letter: pj.drive_letter,
            gpt_type: gpt.as_ref().and_then(|(_, entries)| {
                entries.iter().find(|(lba, _)| lba.checked_mul(sector as u64) == Some(pl.start)).map(|(_, t)| t.clone())
            }),
            mbr_type: if style == Style::Mbr { pj.mbr_type } else { None },
            used_bytes: used_bytes(pj, dj.sector_size),
        })
        .collect();
    DiskInfo {
        size: layout.size,
        sector_size: dj.sector_size,
        style: match style {
            Style::Mbr => "mbr",
            Style::Gpt => "gpt",
            Style::Raw => "raw",
        }
        .into(),
        signature_or_guid,
        partitions,
    }
}

fn used_bytes(pj: &PartitionJson, sector_size: u32) -> Option<u64> {
    (pj.total_clusters > 0 && pj.sectors_per_cluster > 0).then(|| {
        pj.total_clusters
            .saturating_sub(pj.free_clusters)
            .saturating_mul(pj.sectors_per_cluster)
            .saturating_mul(u64::from(sector_size))
    })
}
