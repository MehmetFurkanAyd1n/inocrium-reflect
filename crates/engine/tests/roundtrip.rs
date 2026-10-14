//! Backup → verify → restore round trips on in-memory disks.

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use blockdev::gpt::{self, Gpt, GptEntry};
use blockdev::{mbr, BlockDevice, MemDevice, Slice};
use engine::*;
use irimg::{
    CaptureMode, DiskManifest, GptPartitionInfo, ImageReader, KdfParams, MbrPartitionInfo, PartitionManifest,
    PartitionStyle, WriterOptions,
};
use uuid::Uuid;

const SS: u32 = 512;
const MIB: u64 = 1 << 20;

/// Deterministic noise.
fn fill_noise(buf: &mut [u8], seed: u64) {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
}

fn read(dev: &dyn BlockDevice, r: Range<u64>) -> Vec<u8> {
    let mut buf = vec![0u8; (r.end - r.start) as usize];
    dev.read_at(r.start, &mut buf).unwrap();
    buf
}

fn assert_same(a: &dyn BlockDevice, b: &dyn BlockDevice, r: Range<u64>, what: &str) {
    assert!(read(a, r.clone()) == read(b, r.clone()), "{what}: {r:?} differs");
}

fn part(number: u32, offset: u64, length: u64) -> PartitionManifest {
    PartitionManifest {
        number,
        stream: 0,
        offset,
        length,
        captured: false,
        capture: CaptureMode::Forensic,
        gpt: None,
        mbr: None,
        filesystem: String::new(),
        label: String::new(),
        drive_letter: None,
        used_bytes: None,
        stored_bytes: 0,
        is_system: false,
        is_boot: false,
    }
}

fn disk_manifest(size: u64, layout: PartitionStyle, partitions: Vec<PartitionManifest>) -> DiskManifest {
    DiskManifest {
        index: 0,
        source_number: 1,
        model: "Mem".into(),
        serial: String::new(),
        size,
        sector_size: SS,
        physical_sector_size: 4096,
        layout,
        partitions,
        gaps: vec![],
        raw_regions: vec![],
    }
}

/// 64 MiB GPT disk: p1 1–9 MiB, p2 10–30 MiB, p3 40–48 MiB, noise everywhere except the
/// second half of p1, which is zero.
fn gpt_disk() -> (Arc<MemDevice>, DiskManifest) {
    let size = 64 * MIB;
    let mut data = vec![0u8; size as usize];
    fill_noise(&mut data, 1);
    data[(5 * MIB) as usize..(9 * MIB) as usize].fill(0);

    let layout = [(1, MIB, 8 * MIB), (2, 10 * MIB, 20 * MIB), (3, 40 * MIB, 8 * MIB)];
    let mut partitions = Vec::new();
    let mut entries = Vec::new();
    for (n, offset, length) in layout {
        let info =
            GptPartitionInfo { type_id: gpt::BASIC_DATA, id: Uuid::new_v4(), name: format!("p{n}"), attributes: 0 };
        entries.push(GptEntry {
            type_id: info.type_id,
            id: info.id,
            first_lba: offset / SS as u64,
            last_lba: (offset + length) / SS as u64 - 1,
            attributes: 0,
            name: info.name.clone(),
        });
        let mut p = part(n, offset, length);
        p.gpt = Some(info);
        partitions.push(p);
    }
    let table = Gpt { disk_id: Uuid::new_v4(), sector_size: SS, entry_count: gpt::DEFAULT_ENTRY_COUNT, entries };
    let sectors = table.build(size, None, None);
    data[..sectors.primary.len()].copy_from_slice(&sectors.primary);
    let at = sectors.backup_offset as usize;
    data[at..at + sectors.backup.len()].copy_from_slice(&sectors.backup);

    let style = PartitionStyle::Gpt {
        disk_id: table.disk_id,
        first_usable_lba: sectors.first_usable_lba,
        last_usable_lba: sectors.last_usable_lba,
        max_partition_count: table.entry_count,
    };
    (Arc::new(MemDevice::from_vec(data, SS)), disk_manifest(size, style, partitions))
}

/// 32 MiB MBR disk: p1 1–11 MiB (bootable), p2 12–20 MiB.
fn mbr_disk() -> (Arc<MemDevice>, DiskManifest) {
    let size = 32 * MIB;
    let mut data = vec![0u8; size as usize];
    fill_noise(&mut data, 2);
    let layout = [(1, MIB, 10 * MIB, true), (2, 12 * MIB, 8 * MIB, false)];
    let mut partitions = Vec::new();
    let mut entries = Vec::new();
    for (n, offset, length, bootable) in layout {
        entries.push(mbr::MbrEntry {
            bootable,
            partition_type: 0x07,
            first_lba: (offset / SS as u64) as u32,
            sectors: (length / SS as u64) as u32,
        });
        let mut p = part(n, offset, length);
        p.mbr = Some(MbrPartitionInfo { partition_type: 0x07, bootable, logical: false });
        partitions.push(p);
    }
    let boot_code = [0xEBu8; 440];
    data[..512].copy_from_slice(&mbr::build(0x1234_5678, &entries, Some(&boot_code)));
    (
        Arc::new(MemDevice::from_vec(data, SS)),
        disk_manifest(size, PartitionStyle::Mbr { signature: 0x1234_5678 }, partitions),
    )
}

fn source_partition(disk: &Arc<MemDevice>, d: &DiskManifest, number: u32, used: Option<Extents>) -> SourcePartition {
    let p = d.partition(number).unwrap();
    let device: Arc<dyn BlockDevice> = Arc::new(Slice::new(disk.clone(), p.offset, p.length).unwrap());
    SourcePartition { number, device, used }
}

fn options(password: Option<&str>, split: Option<u64>) -> BackupOptions {
    BackupOptions {
        writer: WriterOptions {
            block_size: irimg::MIN_BLOCK_SIZE,
            compression_level: 1,
            password: password.map(Into::into),
            kdf: KdfParams::insecure_fast(),
            split_size: split,
        },
        comment: "round trip".into(),
        threads: 2,
        ..Default::default()
    }
}

fn filled(size: u64, byte: u8) -> MemDevice {
    MemDevice::from_vec(vec![byte; size as usize], SS)
}

fn gpt_entries(dev: &dyn BlockDevice) -> (gpt::GptHeader, Vec<GptEntry>) {
    let head = read(dev, 0..34 * SS as u64);
    let (header, entries) = gpt::parse_primary(&head, SS).expect("valid primary GPT");
    (header, entries.into_iter().filter(|e| !e.type_id.is_nil()).collect())
}

#[test]
fn gpt_smart_and_forensic() {
    let (disk, d) = gpt_disk();
    // p2 is smart with two used areas; the engine also keeps its first and last MiB.
    let used = Extents::from_ranges([2 * MIB..3 * MIB + 100, 7 * MIB..9 * MIB]);
    let src = SourceDisk {
        device: disk.clone(),
        manifest: d.clone(),
        partitions: vec![source_partition(&disk, &d, 1, None), source_partition(&disk, &d, 2, Some(used.clone()))],
        forensic_gaps: true,
    };
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("gpt.irimg");
    let opts = options(Some("secret"), Some(8 * MIB));
    let out = backup(&dest, &[src], &opts, None, serde_json::Value::Null, &no_progress, &Cancel::new()).unwrap();
    assert!(irimg::image_files(&dest).len() > 1, "split into several files");
    assert!(!checkpoint_path(&dest).exists());

    let m = &out.manifest.disks[0];
    let p = |n| m.partition(n).unwrap();
    assert!(p(1).captured && p(1).capture == CaptureMode::Forensic);
    assert!(p(2).captured && p(2).capture == CaptureMode::Smart);
    assert!(!p(3).captured);
    let gaps: Vec<_> = m.gaps.iter().map(|g| g.offset..g.offset + g.length).collect();
    assert_eq!(gaps[..2], [9 * MIB..10 * MIB, 30 * MIB..40 * MIB]);
    // The zero half of p1 is not stored.
    assert!(p(1).stored_bytes <= 4 * MIB);

    assert!(ImageReader::open(&dest, Some("wrong")).is_err());
    let reader = ImageReader::open(&dest, Some("secret")).unwrap();
    assert_eq!(reader.manifest().comment, "round trip");
    let report = verify(&reader, &no_progress, &Cancel::new()).unwrap();
    assert!(report.ok());
    assert!(report.blocks_checked > 0);

    let smart = Extents::from_ranges(
        used.ranges().iter().cloned().chain([0..MIB, 19 * MIB..20 * MIB]).map(|r| r.start + 10 * MIB..r.end + 10 * MIB),
    );

    // Same size, onto a disk full of garbage.
    let target = filled(64 * MIB, 0xAA);
    let rep = restore_disk(&reader, 0, &target, &RestoreOptions::default(), &no_progress, &Cancel::new()).unwrap();
    assert_eq!(rep.extended, None);
    assert_same(&target, &*disk, MIB..9 * MIB, "forensic partition (incl. zero-filled half)");
    for r in smart.ranges() {
        assert_same(&target, &*disk, r.clone(), "smart partition");
    }
    for g in &gaps {
        assert_same(&target, &*disk, g.clone(), "gap");
    }
    assert!(read(&target, 40 * MIB..48 * MIB).iter().all(|&b| b == 0xAA), "uncaptured partition untouched");
    let (header, entries) = gpt_entries(&target);
    assert_eq!(entries.len(), 2);
    assert_eq!((entries[0].id, entries[1].id), (p(1).gpt.as_ref().unwrap().id, p(2).gpt.as_ref().unwrap().id));
    assert_eq!(header.alternate_lba, 64 * MIB / SS as u64 - 1);
    let backup_header = read(&target, 64 * MIB - SS as u64..64 * MIB);
    assert_eq!(gpt::GptHeader::decode(&backup_header).unwrap().my_lba, 64 * MIB / SS as u64 - 1);

    // Larger disk, last partition grown, new identity.
    let big = filled(96 * MIB, 0);
    let opts = RestoreOptions { extend_last: true, rewrite_identity: true, ..Default::default() };
    let rep = restore_disk(&reader, 0, &big, &opts, &no_progress, &Cancel::new()).unwrap();
    assert_eq!(rep.extended, Some(2));
    assert_eq!(rep.layout.partition(2).unwrap().length, 85 * MIB);
    let (header, entries) = gpt_entries(&big);
    assert_eq!(entries[1].first_lba, 10 * MIB / SS as u64);
    assert_eq!(entries[1].last_lba, 95 * MIB / SS as u64 - 1);
    assert_eq!(header.alternate_lba, 96 * MIB / SS as u64 - 1);
    assert!(header.last_usable_lba >= entries[1].last_lba);
    assert_ne!(entries[0].id, p(1).gpt.as_ref().unwrap().id);
    assert_same(&big, &*disk, MIB..9 * MIB, "forensic partition on larger disk");

    // Smaller disks are refused.
    assert!(restore_disk(&reader, 0, &filled(32 * MIB, 0), &RestoreOptions::default(), &no_progress, &Cancel::new())
        .is_err());

    // One partition into a partition-sized target.
    let target = filled(8 * MIB, 0xAA);
    restore_partition(&reader, 0, 1, &target, &RestoreOptions::default(), &no_progress, &Cancel::new()).unwrap();
    assert!(read(&target, 0..8 * MIB) == read(&*disk, MIB..9 * MIB));
    assert!(
        restore_partition(&reader, 0, 3, &target, &RestoreOptions::default(), &no_progress, &Cancel::new()).is_err()
    );
}

#[test]
fn mbr_extend() {
    let (disk, d) = mbr_disk();
    let src = SourceDisk {
        device: disk.clone(),
        manifest: d.clone(),
        partitions: vec![source_partition(&disk, &d, 1, None), source_partition(&disk, &d, 2, None)],
        forensic_gaps: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("mbr.irimg");
    backup(&dest, &[src], &options(None, None), None, serde_json::Value::Null, &no_progress, &Cancel::new()).unwrap();
    let reader = ImageReader::open(&dest, None).unwrap();
    assert!(verify(&reader, &no_progress, &Cancel::new()).unwrap().ok());

    let target = filled(48 * MIB, 0);
    let opts = RestoreOptions { extend_last: true, ..Default::default() };
    let rep = restore_disk(&reader, 0, &target, &opts, &no_progress, &Cancel::new()).unwrap();
    assert_eq!(rep.extended, Some(2));
    let sector0 = read(&target, 0..512);
    assert_eq!(mbr::signature(&sector0), 0x1234_5678);
    assert!(sector0[..440].iter().all(|&b| b == 0xEB), "boot code kept");
    let entries = mbr::entries(&sector0);
    let (e1, e2) = (entries[0].unwrap(), entries[1].unwrap());
    assert!(e1.bootable && !e2.bootable);
    assert_eq!(e1.sectors as u64, 10 * MIB / SS as u64);
    assert_eq!(e2.first_lba as u64, 12 * MIB / SS as u64);
    assert_eq!(e2.sectors as u64, 36 * MIB / SS as u64);
    assert_same(&target, &*disk, MIB..11 * MIB, "p1");
    assert_same(&target, &*disk, 12 * MIB..20 * MIB, "p2");
    // Not a forensic image: unpartitioned space is not restored.
    assert!(read(&target, 11 * MIB..12 * MIB).iter().all(|&b| b == 0));
}

/// Cancels the backup after a number of reads.
struct CancelAfter {
    inner: Arc<dyn BlockDevice>,
    reads: AtomicU64,
    limit: u64,
    cancel: Cancel,
}

impl BlockDevice for CancelAfter {
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
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 >= self.limit {
            self.cancel.cancel();
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

#[test]
fn resume_after_cancel() {
    let (disk, d) = mbr_disk();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("resume.irimg");
    let opts = options(Some("pw"), None);

    let cancel = Cancel::new();
    let wrap = |s: SourcePartition, limit, cancel: &Cancel| -> (SourcePartition, Arc<CancelAfter>) {
        let dev = Arc::new(CancelAfter { inner: s.device, reads: AtomicU64::new(0), limit, cancel: cancel.clone() });
        (SourcePartition { number: s.number, device: dev.clone(), used: s.used }, dev)
    };
    let (p1, first) = wrap(source_partition(&disk, &d, 1, None), 12, &cancel);
    let src = SourceDisk { device: disk.clone(), manifest: d.clone(), partitions: vec![p1], forensic_gaps: false };
    let caller = serde_json::json!({ "definition": "abc" });
    let err = backup(&dest, &[src], &opts, None, caller.clone(), &no_progress, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    let read_before = first.reads.load(Ordering::SeqCst);
    assert!(read_before < 40, "cancelled before the end ({read_before} reads)");

    let ckpt = load_checkpoint(&dest).unwrap().expect("checkpoint written");
    assert_eq!(checkpoint_caller_state(&ckpt), &caller);

    // A different source layout is refused.
    let changed = SourceDisk {
        device: disk.clone(),
        manifest: d.clone(),
        partitions: vec![source_partition(&disk, &d, 2, None)],
        forensic_gaps: false,
    };
    assert!(backup(&dest, &[changed], &opts, Some(&ckpt), caller.clone(), &no_progress, &Cancel::new()).is_err());

    // Matching geometry is not sufficient when the source identity or partition type changed.
    for variant in 0..4 {
        let mut manifest = d.clone();
        match variant {
            0 => manifest.layout = PartitionStyle::Mbr { signature: 0xDEAD_BEEF },
            1 => manifest.serial = "replacement disk".into(),
            2 => manifest.source_number += 1,
            _ => manifest.partitions[0].mbr.as_mut().unwrap().partition_type = 0x0B,
        }
        let changed = SourceDisk {
            device: disk.clone(),
            manifest,
            partitions: vec![source_partition(&disk, &d, 1, None)],
            forensic_gaps: false,
        };
        assert!(backup(&dest, &[changed], &opts, Some(&ckpt), caller.clone(), &no_progress, &Cancel::new()).is_err());
    }

    let (p1, second) = wrap(source_partition(&disk, &d, 1, None), u64::MAX, &Cancel::new());
    let src = SourceDisk { device: disk.clone(), manifest: d.clone(), partitions: vec![p1], forensic_gaps: false };
    backup(&dest, &[src], &opts, Some(&ckpt), caller, &no_progress, &Cancel::new()).unwrap();
    assert!(!checkpoint_path(&dest).exists());
    let reread = second.reads.load(Ordering::SeqCst);
    assert!(reread + read_before >= 40 && reread < 40, "blocks already in the image were skipped ({reread} reads)");

    let reader = ImageReader::open(&dest, Some("pw")).unwrap();
    assert!(verify(&reader, &no_progress, &Cancel::new()).unwrap().ok());
    let target = filled(32 * MIB, 0);
    restore_partition(&reader, 0, 1, &target, &RestoreOptions::default(), &no_progress, &Cancel::new()).unwrap();
    assert!(read(&target, 0..10 * MIB) == read(&*disk, MIB..11 * MIB));
}

/// A minimal valid image at `path`.
fn tiny_image(path: &Path) {
    let disk = Arc::new(filled(MIB, 7));
    let src = SourceDisk {
        device: disk,
        manifest: disk_manifest(MIB, PartitionStyle::Raw, vec![]),
        partitions: vec![],
        forensic_gaps: true,
    };
    backup(path, &[src], &options(None, None), None, serde_json::Value::Null, &no_progress, &Cancel::new()).unwrap();
}

#[test]
fn retention_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let name = |s: &str| -> PathBuf { dir.path().join(format!("sys-20261014-{s}.irimg")) };
    let all = [name("000001"), name("000002"), name("000003"), name("000004")];
    for p in &all {
        tiny_image(p);
    }
    tiny_image(&dir.path().join("other-20261014-000000.irimg"));
    std::fs::write(checkpoint_path(&all[1]), b"{}").unwrap(); // incomplete
    std::fs::write(dir.path().join("sys-notes.txt"), b"x").unwrap();

    let found = find_images(dir.path(), "sys").unwrap();
    assert_eq!(found.iter().map(|i| i.path.clone()).collect::<Vec<_>>(), all);
    assert!(found[1].incomplete && !found[0].incomplete);

    let now = chrono::Utc::now().timestamp();
    let deleted = apply_retention(dir.path(), "sys", Retention::KeepLast(2), now, Some(&all[3])).unwrap();
    assert_eq!(deleted, [all[0].clone()]);
    assert!(!all[0].exists() && all[1].exists() && all[2].exists() && all[3].exists());

    // Nothing is old enough yet.
    assert!(apply_retention(dir.path(), "sys", Retention::KeepDays(1), now, None).unwrap().is_empty());
    let later = now + 2 * 86_400;
    let deleted = apply_retention(dir.path(), "sys", Retention::KeepDays(1), later, None).unwrap();
    assert_eq!(deleted, [all[2].clone(), all[3].clone()]);
    assert!(all[1].exists(), "incomplete image kept");
    assert!(dir.path().join("other-20261014-000000.irimg").exists());

    let json = serde_json::to_string(&Retention::KeepDays(7)).unwrap();
    assert_eq!(json, r#"{"kind":"keep_days","value":7}"#);
}
