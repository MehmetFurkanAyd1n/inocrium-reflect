//! Physical disk inventory and resolution of saved disk selections.

use engine::win;
use ipc::proto::{self, DiskStyle, RpcError, NOT_FOUND};
use uuid::{uuid, Uuid};
use winio::{DiskInfo, LayoutStyle, PartitionInfo, PartitionKind};

use crate::store::internal;

const GPT_BASIC_DATA: Uuid = uuid!("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7");
const GPT_ESP: Uuid = uuid!("c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
const GPT_MSR: Uuid = uuid!("e3c9e316-0b5c-4db8-817d-f92df00215ae");
const GPT_RECOVERY: Uuid = uuid!("de94bba4-06d1-4d40-a16a-bfd50179d6ac");
const GPT_LDM_META: Uuid = uuid!("5808c8aa-7e8f-42e0-85d2-e1e90434cfb3");
const GPT_LDM_DATA: Uuid = uuid!("af9b60a0-1431-4f62-bc68-3311714a69ad");
const GPT_STORAGE_SPACES: Uuid = uuid!("e75caf8f-f680-4cee-afa3-b001e56efc2d");
const GPT_LINUX_DATA: Uuid = uuid!("0fc63daf-8483-4772-8e79-3d69d8477de4");
const GPT_LINUX_SWAP: Uuid = uuid!("0657fd6d-a4ab-43c4-84e5-0933c84b4f4f");

/// Every physical disk the service can see, in the wire format, except the disks of mounted
/// images (`mounted` lists the service's own; direct CLI mounts are recognized by their model).
pub fn list(mounted: &[u32]) -> Result<Vec<proto::Disk>, RpcError> {
    Ok(winio::list_disks()
        .map_err(internal)?
        .iter()
        .filter(|d| !mounted.contains(&d.number) && !is_image_disk(d))
        .map(to_proto)
        .collect())
}

/// A loopback iSCSI disk exposing an image (see `engine::win::mount`).
fn is_image_disk(disk: &DiskInfo) -> bool {
    disk.bus_type.eq_ignore_ascii_case("iSCSI") && disk.model.trim() == "Image Disk"
}

/// The identity clients store in a definition: the GPT disk GUID or the MBR signature.
pub fn disk_id(disk: &DiskInfo) -> String {
    match &disk.layout.style {
        LayoutStyle::Gpt { disk_id, .. } => disk_id.to_string(),
        LayoutStyle::Mbr { signature } => format!("{signature:08X}"),
        LayoutStyle::Raw => String::new(),
    }
}

pub fn to_proto(disk: &DiskInfo) -> proto::Disk {
    let style = match disk.layout.style {
        LayoutStyle::Gpt { .. } => DiskStyle::Gpt,
        LayoutStyle::Mbr { .. } => DiskStyle::Mbr,
        LayoutStyle::Raw => DiskStyle::Raw,
    };
    proto::Disk {
        number: disk.number,
        model: disk.model.trim().to_owned(),
        serial: disk.serial.trim().to_owned(),
        size: disk.size,
        sector_size: disk.sector_size,
        style,
        disk_id: disk_id(disk),
        is_system: disk.is_system,
        partitions: disk.layout.partitions.iter().map(partition).collect(),
    }
}

fn partition(p: &PartitionInfo) -> proto::Partition {
    let volume = p.volume.as_ref();
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let (type_name, is_esp, bootable) = match &p.kind {
        PartitionKind::Gpt { type_id, .. } => (gpt_type_name(type_id), *type_id == GPT_ESP, false),
        PartitionKind::Mbr { partition_type, bootable, .. } => {
            (mbr_type_name(*partition_type), *partition_type == 0xEF, *bootable)
        }
    };
    proto::Partition {
        number: p.number,
        offset: p.offset,
        length: p.length,
        type_name,
        file_system: volume.and_then(|v| non_empty(&v.filesystem)),
        label: volume.and_then(|v| non_empty(&v.label)),
        letter: volume.and_then(|v| v.drive_letters.first()).map(|c| c.to_string()),
        used_bytes: volume.filter(|v| v.total_bytes > 0).map(|v| v.total_bytes.saturating_sub(v.free_bytes)),
        is_system: is_esp || bootable || volume.is_some_and(|v| v.is_system),
        is_boot: volume.is_some_and(|v| v.is_windows),
        is_esp,
    }
}

fn gpt_type_name(id: &Uuid) -> String {
    let name = match *id {
        GPT_BASIC_DATA => "Basic data",
        GPT_ESP => "EFI system",
        GPT_MSR => "Microsoft reserved",
        GPT_RECOVERY => "Recovery",
        GPT_LDM_META => "LDM metadata",
        GPT_LDM_DATA => "LDM data",
        GPT_STORAGE_SPACES => "Storage Spaces",
        GPT_LINUX_DATA => "Linux filesystem",
        GPT_LINUX_SWAP => "Linux swap",
        _ => return format!("GPT {id}"),
    };
    name.to_owned()
}

fn mbr_type_name(kind: u8) -> String {
    let name = match kind {
        0x01 | 0x04 | 0x06 | 0x0E => "FAT",
        0x07 => "NTFS/exFAT",
        0x0B | 0x0C => "FAT32",
        0x12 | 0x27 => "Recovery",
        0x42 => "LDM",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        _ => return format!("MBR 0x{kind:02X}"),
    };
    name.to_owned()
}

/// Maps saved selections onto the disks present now. Disk numbers change between boots, so a
/// stored identity wins over the number; a selection whose identity is gone is an error rather
/// than a backup of whatever disk now carries that number.
pub fn resolve(selections: &[proto::DiskSelection]) -> Result<Vec<win::DiskSelection>, RpcError> {
    resolve_against(selections, &winio::list_disks().map_err(internal)?)
}

fn resolve_against(
    selections: &[proto::DiskSelection],
    disks: &[DiskInfo],
) -> Result<Vec<win::DiskSelection>, RpcError> {
    if selections.is_empty() {
        return Err(RpcError::invalid_params("no disks selected"));
    }
    let mut out: Vec<win::DiskSelection> = Vec::with_capacity(selections.len());
    for sel in selections {
        let wanted = sel.disk_id.trim();
        let disk = if wanted.is_empty() {
            disks.iter().find(|d| d.number == sel.number)
        } else {
            disks.iter().find(|d| d.number == sel.number && same_id(&disk_id(d), wanted)).or_else(|| {
                let mut matches = disks.iter().filter(|d| same_id(&disk_id(d), wanted));
                matches.next().filter(|_| matches.next().is_none())
            })
        };
        let Some(disk) = disk else {
            let what = if wanted.is_empty() { format!("disk {}", sel.number) } else { format!("disk {wanted}") };
            return Err(RpcError::new(NOT_FOUND, format!("{what} is not attached")));
        };
        let partitions = match &sel.partitions {
            None => None,
            Some(list) if list.is_empty() => {
                return Err(RpcError::invalid_params(format!("disk {}: empty partition selection", disk.number)))
            }
            Some(list) => {
                if let Some(n) = list.iter().find(|n| !disk.layout.partitions.iter().any(|p| p.number == **n)) {
                    return Err(RpcError::new(NOT_FOUND, format!("disk {} has no partition {n}", disk.number)));
                }
                let mut list = list.clone();
                list.sort_unstable();
                list.dedup();
                Some(list)
            }
        };
        if out.iter().any(|s| s.disk == disk.number) {
            return Err(RpcError::invalid_params(format!("disk {} selected twice", disk.number)));
        }
        out.push(win::DiskSelection { disk: disk.number, partitions });
    }
    Ok(out)
}

fn same_id(a: &str, b: &str) -> bool {
    !a.is_empty()
        && a.trim_matches(|c| c == '{' || c == '}').eq_ignore_ascii_case(b.trim_matches(|c| c == '{' || c == '}'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winio::{DiskLayout, VolumeInfo};

    fn disk(number: u32, style: LayoutStyle, parts: &[u32]) -> DiskInfo {
        DiskInfo {
            number,
            size: 1 << 30,
            sector_size: 512,
            physical_sector_size: 4096,
            model: " Test ".into(),
            serial: String::new(),
            bus_type: "NVMe".into(),
            removable: false,
            is_virtual: false,
            offline: false,
            read_only: false,
            is_system: false,
            layout: DiskLayout {
                style,
                partitions: parts
                    .iter()
                    .map(|&n| PartitionInfo {
                        number: n,
                        offset: u64::from(n) << 20,
                        length: 1 << 20,
                        kind: PartitionKind::Gpt {
                            type_id: GPT_BASIC_DATA,
                            id: Uuid::nil(),
                            name: String::new(),
                            attributes: 0,
                        },
                        volume: None,
                    })
                    .collect(),
            },
        }
    }

    fn gpt(id: Uuid) -> LayoutStyle {
        LayoutStyle::Gpt { disk_id: id, starting_usable_offset: 0, usable_length: 0, max_partition_count: 128 }
    }

    fn sel(number: u32, disk_id: &str, partitions: Option<Vec<u32>>) -> proto::DiskSelection {
        proto::DiskSelection { number, disk_id: disk_id.into(), partitions }
    }

    #[test]
    fn identity_beats_number() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let disks =
            [disk(0, gpt(a), &[1, 2]), disk(1, gpt(b), &[1]), disk(2, LayoutStyle::Mbr { signature: 0xABCD }, &[1])];
        let r = resolve_against(&[sel(0, &b.to_string(), None)], &disks).unwrap();
        assert_eq!(r[0].disk, 1);
        let r = resolve_against(&[sel(7, "0000ABCD", Some(vec![1, 1]))], &disks).unwrap();
        assert_eq!((r[0].disk, r[0].partitions.clone()), (2, Some(vec![1])));
        let r = resolve_against(&[sel(0, "", Some(vec![2]))], &disks).unwrap();
        assert_eq!(r[0].disk, 0);
        assert!(resolve_against(&[sel(0, &Uuid::from_u128(9).to_string(), None)], &disks).is_err());
        assert!(resolve_against(&[sel(0, "", Some(vec![]))], &disks).is_err());
        assert!(resolve_against(&[sel(0, "", Some(vec![5]))], &disks).is_err());
        assert!(resolve_against(&[sel(0, "", None), sel(9, &a.to_string(), None)], &disks).is_err());
        assert!(resolve_against(&[], &disks).is_err());
    }

    #[test]
    fn wire_format() {
        let mut d = disk(3, LayoutStyle::Mbr { signature: 0x1234 }, &[1]);
        d.layout.partitions[0].kind = PartitionKind::Mbr { partition_type: 0x07, bootable: true, logical: false };
        d.layout.partitions[0].volume = Some(VolumeInfo {
            guid_path: r"\\?\Volume{x}\".into(),
            drive_letters: vec!['C'],
            mount_points: vec![r"C:\".into()],
            filesystem: "NTFS".into(),
            label: String::new(),
            total_bytes: 100,
            free_bytes: 40,
            is_windows: true,
            is_system: false,
        });
        let p = to_proto(&d);
        assert_eq!((p.style, p.disk_id.as_str(), p.model.as_str()), (DiskStyle::Mbr, "00001234", "Test"));
        let part = &p.partitions[0];
        assert_eq!(part.letter.as_deref(), Some("C"));
        assert_eq!(part.label, None);
        assert_eq!(part.used_bytes, Some(60));
        assert!(part.is_system && part.is_boot && !part.is_esp);
        assert_eq!(part.type_name, "NTFS/exFAT");
    }
}
