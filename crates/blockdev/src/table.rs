//! Partition tables regenerated from an image manifest, for disk views, restores and VHDX
//! export. Only captured partitions are listed; partitions keep their original offsets.

use irimg::{DiskManifest, PartitionManifest, PartitionStyle};
use uuid::Uuid;

use crate::gpt::{self, Gpt, GptEntry};
use crate::mbr;

/// The sectors holding the partition table of image disk `d` laid out on a disk of `disk_size`
/// bytes (at least the original size), as `(offset, data)` pairs.
///
/// `track0` is the captured start of the disk, if any: its boot code and anything after the
/// GPT entry array are preserved. For GPT the backup table moves to the end of the new disk
/// and the usable range grows with it. With `rewrite_identity` the MBR signature or the GPT
/// disk and partition GUIDs are replaced with fresh ones.
pub fn disk_table(
    d: &DiskManifest,
    track0: Option<&[u8]>,
    disk_size: u64,
    rewrite_identity: bool,
) -> Vec<(u64, Vec<u8>)> {
    if !valid_sector_size(d.sector_size) || !valid_gpt_geometry(d) {
        return vec![];
    }
    let captured: Vec<&PartitionManifest> = d.captured_partitions().collect();
    let track0 = track0.filter(|s| mbr::has_boot_signature(s));
    match &d.layout {
        PartitionStyle::Gpt { disk_id, first_usable_lba, max_partition_count, .. } => {
            let fresh = |id: Uuid| if rewrite_identity { Uuid::new_v4() } else { id };
            let ss = d.sector_size as u64;
            let entries: Vec<GptEntry> = captured
                .iter()
                .filter_map(|p| {
                    let g = p.gpt.as_ref()?;
                    Some(GptEntry {
                        type_id: g.type_id,
                        id: fresh(g.id),
                        first_lba: p.offset / ss,
                        last_lba: (p.offset.saturating_add(p.length) / ss).saturating_sub(1),
                        attributes: g.attributes,
                        name: g.name.clone(),
                    })
                })
                .collect();
            let table = Gpt {
                disk_id: fresh(*disk_id),
                sector_size: d.sector_size,
                entry_count: (*max_partition_count).max(gpt::DEFAULT_ENTRY_COUNT).max(entries.len() as u32),
                entries,
            };
            // Allocate only the table, while preserving the original usable boundary in both headers.
            let mut sectors = table.build(disk_size, None, track0);
            let first = (*first_usable_lba).max(sectors.first_usable_lba);
            for header in [
                &mut sectors.primary[ss as usize..2 * ss as usize],
                &mut sectors.backup[(table.entry_array_sectors() * ss) as usize..],
            ] {
                let mut h = gpt::GptHeader::decode(header).expect("newly built GPT header");
                h.first_usable_lba = first;
                header.copy_from_slice(&h.encode(d.sector_size));
            }
            // Leave whatever followed the entry array in TRACK0 alone.
            let head = (2 + table.entry_array_sectors()) * ss;
            let mut primary = sectors.primary;
            primary.truncate(head as usize);
            vec![(0, primary), (sectors.backup_offset, sectors.backup)]
        }
        PartitionStyle::Mbr { signature } => {
            vec![(0, patched_mbr(d, &captured, track0, *signature, rewrite_identity).to_vec())]
        }
        PartitionStyle::Raw => vec![],
    }
}

/// Sector 0 of an MBR disk with entries for uncaptured primaries removed. Extended containers
/// and the EBR chain are left as they were.
fn patched_mbr(
    d: &DiskManifest,
    captured: &[&PartitionManifest],
    track0: Option<&[u8]>,
    signature: u32,
    rewrite_identity: bool,
) -> [u8; 512] {
    let ss = d.sector_size as u64;
    let mut sector = match track0 {
        Some(s) => {
            let mut sector: [u8; 512] = s[..512].try_into().unwrap();
            for (i, e) in mbr::entries(&sector).into_iter().enumerate() {
                let Some(mut e) = e else { continue };
                if mbr::is_extended(e.partition_type) {
                    continue;
                }
                // Keep captured primaries, with the manifest's length (it may have been grown).
                match captured.iter().find(|p| p.offset == e.first_lba as u64 * ss) {
                    Some(p) => {
                        e.sectors = (p.length / ss).min(u32::MAX as u64) as u32;
                        mbr::set_entry(&mut sector, i, Some(&e));
                    }
                    None => mbr::set_entry(&mut sector, i, None),
                }
            }
            sector
        }
        None => {
            let entries: Vec<mbr::MbrEntry> = captured
                .iter()
                .filter(|p| !p.mbr.is_some_and(|m| m.logical))
                .map(|p| mbr::MbrEntry {
                    bootable: p.mbr.is_some_and(|m| m.bootable),
                    partition_type: p.mbr.map_or(0x07, |m| m.partition_type),
                    first_lba: (p.offset / ss) as u32,
                    sectors: (p.length / ss) as u32,
                })
                .collect();
            mbr::build(signature, &entries, None)
        }
    };
    mbr::set_signature(&mut sector, if rewrite_identity { mbr::random_signature() } else { signature });
    sector
}

/// Sector sizes a partition table can be laid out with (the manifest is untrusted).
pub(crate) fn valid_sector_size(ss: u32) -> bool {
    (512..=65536).contains(&ss) && ss.is_power_of_two()
}

pub(crate) fn valid_gpt_geometry(d: &DiskManifest) -> bool {
    match d.layout {
        PartitionStyle::Gpt { max_partition_count, first_usable_lba, .. } => {
            // Limit untrusted arrays to 64 MiB; reject unsupported geometry instead of truncating it.
            max_partition_count <= 524288
                && d.partitions.len() <= 524288
                && d.sector_size != 0
                && first_usable_lba <= d.size / u64::from(d.sector_size)
        }
        _ => true,
    }
}
