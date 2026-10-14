use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use blockdev::BlockDevice;

use crate::testutil::*;
use crate::{Error, MrimgxImage};

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn read_all(dev: &dyn BlockDevice) -> Vec<u8> {
    let mut buf = vec![0xCC; dev.size() as usize];
    dev.read_at(0, &mut buf).unwrap();
    buf
}

/// Checks every partition of `img` against `spec`.
fn check_partitions(img: &MrimgxImage, spec: &ImageSpec) {
    for (d, disk) in spec.disks.iter().enumerate() {
        for (p, part) in disk.parts.iter().enumerate() {
            let dev = img.partition_device(d, p).unwrap();
            assert_eq!(dev.size(), part.content.len() as u64);
            assert_eq!(read_all(&*dev), part.expected(), "disk {d} partition {p}");
        }
    }
}

fn open_single(spec: &ImageSpec, password: Option<&str>) -> (tempfile::TempDir, MrimgxImage) {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "image.mrimgx", &single_file(spec).0);
    let img = MrimgxImage::open(&path, password).unwrap();
    (dir, img)
}

#[test]
fn image_is_send_and_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<MrimgxImage>();
}

#[test]
fn full_image() {
    let spec = sample_spec();
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "image.mrimgx", &single_file(&spec).0);
    assert!(MrimgxImage::is_mrimgx(&path));
    assert!(!MrimgxImage::needs_password(&path).unwrap());
    let img = MrimgxImage::open(&path, None).unwrap();
    check_partitions(&img, &spec);

    let info = img.info();
    assert_eq!(info.image_id, "0AD2FD9362D493B1");
    assert_eq!(info.kind, "full");
    assert_eq!(info.created.as_deref(), Some("2023-11-14T22:13:20Z"));
    assert_eq!(info.comment, "synthetic test image");
    assert_eq!(info.hostname, "TESTPC");
    assert!(!info.encrypted);
    let disk = &info.disks[0];
    assert_eq!((disk.size, disk.sector_size, disk.style.as_str()), (256 * 1024, 512, "gpt"));
    assert_eq!(disk.signature_or_guid, "6A578E4D-7B5C-4CC1-85D3-FFCA233A9B93");
    let p = &disk.partitions[1];
    assert_eq!((p.index, p.number, p.offset, p.length), (1, 2, 128 * 1024, 24 * 1024));
    assert_eq!((p.filesystem.as_str(), p.label.as_str(), p.drive_letter), ("FAT32", "VOL2", Some('C')));
    assert_eq!(p.gpt_type.as_deref(), Some("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"));
    assert_eq!(p.mbr_type, None);
    assert_eq!(p.used_bytes, Some(60 * 8 * 512));
    serde_json::to_string(info).unwrap();

    let v = img.verify(&mut |_, _| true).unwrap();
    // 9 + 3 reserved + 4 data blocks are stored.
    assert_eq!((v.ok, v.bad_blocks, v.checked_blocks), (true, 0, 16));

    assert!(matches!(img.disk_device(1), Err(Error::NotFound(_))));
    assert!(matches!(img.partition_device(0, 2), Err(Error::NotFound(_))));
}

#[test]
fn zstd_compressed() {
    let spec = ImageSpec { compress: true, ..sample_spec() };
    let (bytes, _) = single_file(&spec);
    let raw: usize = spec.disks[0].parts.iter().map(|p| p.content.len()).sum();
    assert!(bytes.len() < raw, "blocks should be compressed");
    let (_dir, img) = open_single(&spec, None);
    check_partitions(&img, &spec);
    assert!(img.verify(&mut |_, _| true).unwrap().ok);
}

#[test]
fn encrypted() {
    for (compress, aes) in [(true, "aes-256"), (false, "aes-128"), (false, "aes-192")] {
        let spec = ImageSpec { compress, password: Some(("s3cret", aes)), ..sample_spec() };
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "enc.mrimgx", &single_file(&spec).0);
        assert!(MrimgxImage::needs_password(&path).unwrap());
        assert!(matches!(MrimgxImage::open(&path, None), Err(Error::PasswordRequired)));
        let img = MrimgxImage::open(&path, Some("s3cret")).unwrap();
        assert!(img.info().encrypted);
        check_partitions(&img, &spec);
        assert!(img.verify(&mut |_, _| true).unwrap().ok);
    }
}

#[test]
fn wrong_password() {
    let spec = ImageSpec { password: Some(("right", "aes-256")), ..sample_spec() };
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "enc.mrimgx", &single_file(&spec).0);
    assert!(matches!(MrimgxImage::open(&path, Some("wrong")), Err(Error::WrongPassword)));
}

#[test]
fn split_set() {
    let spec = ImageSpec { compress: true, ..sample_spec() };
    let mut writers = [
        FileWriter::new(&spec, FileMeta { split: true, ..FileMeta::FULL }),
        FileWriter::new(&spec, FileMeta { file_number: 1, ..FileMeta::FULL }),
    ];
    let index = store_all(&spec, &mut writers, |n| n % 2);
    let [w0, w1] = writers;
    let dir = tempfile::tempdir().unwrap();
    let first = write(dir.path(), "SET-00-00.mrimgx", &w0.finish(None));
    let last = write(dir.path(), "SET-01-00.MRIMGX", &w1.finish(Some(&index)));
    write(dir.path(), "junk.mrimgx", b"not an image");

    check_partitions(&MrimgxImage::open(&last, None).unwrap(), &spec);
    // Opening the data-only first part finds the part holding the index.
    check_partitions(&MrimgxImage::open(&first, None).unwrap(), &spec);

    std::fs::remove_file(&first).unwrap();
    assert!(matches!(MrimgxImage::open(&last, None), Err(Error::MissingFile(_))));
}

#[test]
fn cross_block_reads_and_holes() {
    let spec = sample_spec();
    let (_dir, img) = open_single(&spec, None);
    let part = &spec.disks[0].parts[0];
    let expected = part.expected();
    // Block 3 is a hole.
    assert!(expected[3 * 4096..4 * 4096].iter().all(|&b| b == 0));
    assert!(part.content[3 * 4096..4 * 4096].iter().any(|&b| b != 0));

    let dev = img.partition_device(0, 0).unwrap();
    for (off, len) in [(4090, 20), (1, 4096 * 3), (3 * 4096 - 5, 4096 + 10), (40 * 1024 - 7, 7), (0, 0), (8191, 1)] {
        let mut buf = vec![0xCC; len];
        dev.read_at(off as u64, &mut buf).unwrap();
        assert_eq!(buf, expected[off..off + len], "offset {off} len {len}");
    }
    let mut buf = [0; 2];
    assert_eq!(dev.read_at(40 * 1024 - 1, &mut buf).unwrap_err().kind(), ErrorKind::InvalidInput);
    assert_eq!(dev.write_at(0, &buf).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert!(dev.read_only());
}

#[test]
fn corrupted_block_is_detected() {
    let spec = sample_spec();
    let (mut bytes, index) = single_file(&spec);
    let elem = index[0][0].blocks[5];
    bytes[elem.pos as usize + 10] ^= 0xFF;
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "bad.mrimgx", &bytes);
    let img = MrimgxImage::open(&path, None).unwrap();

    let mut calls = 0;
    let v = img
        .verify(&mut |done, total| {
            calls += 1;
            assert!(done <= total);
            true
        })
        .unwrap();
    assert_eq!((v.ok, v.bad_blocks, v.checked_blocks, calls), (false, 1, 16, 16));

    let dev = img.partition_device(0, 0).unwrap();
    let mut buf = vec![0; 100];
    assert_eq!(dev.read_at(5 * 4096 + 50, &mut buf).unwrap_err().kind(), ErrorKind::InvalidData);
    dev.read_at(4 * 4096, &mut buf).unwrap();

    let v = img.verify(&mut |done, _| done < 3).unwrap();
    assert!(!v.ok && v.checked_blocks == 3 && v.message.contains("cancelled"));
}

#[test]
fn disk_device_layout() {
    for gpt in [true, false] {
        let mut spec = sample_spec();
        let disk = &mut spec.disks[0];
        disk.gpt = gpt;
        if !gpt {
            disk.signature.clear();
        }
        let mut ebr = [0x5A; 512];
        ebr[510..].copy_from_slice(&[0x55, 0xAA]);
        disk.ept.push((200 * 1024, ebr));
        let (_dir, img) = open_single(&spec, None);

        let disk = &spec.disks[0];
        let mut expected = vec![0; disk.size as usize];
        let track0 = disk.track0();
        expected[..track0.len()].copy_from_slice(&track0);
        expected[200 * 1024..200 * 1024 + 512].copy_from_slice(&ebr);
        for p in &disk.parts {
            let at = p.start as usize;
            expected[at..at + p.content.len()].copy_from_slice(&p.expected());
        }
        let dev = img.disk_device(0).unwrap();
        assert_eq!((dev.size(), dev.block_size()), (disk.size, 512));
        assert_eq!(read_all(&*dev), expected);
        // A read spanning track 0, the gap and the first partition.
        let mut buf = vec![0xCC; 40 * 1024];
        dev.read_at(30 * 1024, &mut buf).unwrap();
        assert_eq!(buf, expected[30 * 1024..70 * 1024]);

        let info = &img.info().disks[0];
        if gpt {
            assert_eq!(info.style, "gpt");
        } else {
            assert_eq!((info.style.as_str(), info.signature_or_guid.as_str()), ("mbr", "12345678"));
            assert_eq!(info.partitions[0].mbr_type, Some(7));
            assert_eq!(info.partitions[0].gpt_type, None);
        }
    }
}

#[test]
fn garbled_files_do_not_panic() {
    let spec = ImageSpec { compress: true, ..sample_spec() };
    let (bytes, _) = single_file(&spec);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fuzz.mrimgx");
    let try_open = |data: &[u8]| {
        std::fs::write(&path, data).unwrap();
        let _ = MrimgxImage::is_mrimgx(&path);
        let _ = MrimgxImage::needs_password(&path);
        if let Ok(img) = MrimgxImage::open(&path, Some("x")) {
            let _ = img.verify(&mut |_, _| true);
            for d in 0..img.info().disks.len() {
                let mut buf = vec![0; 64 * 1024];
                if let Ok(dev) = img.disk_device(d) {
                    let n = buf.len().min(dev.size() as usize);
                    let _ = dev.read_at(0, &mut buf[..n]);
                }
            }
        }
    };
    for len in [0, 1, 19, 20, 21, 100, bytes.len() / 2, bytes.len() - 21, bytes.len() - 20, bytes.len() - 1] {
        try_open(&bytes[..len]);
    }
    try_open(&bytes[bytes.len() - 20..]);
    // Corrupt bytes in the metadata area (everything after the data blocks) one at a time.
    let mut seed = 0x9E37_79B9_u32;
    let meta_start = bytes.len() - 4096.min(bytes.len());
    for _ in 0..400 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let mut data = bytes.clone();
        let at = meta_start + seed as usize % (bytes.len() - meta_start);
        data[at] = (seed >> 24) as u8;
        try_open(&data);
    }
    // Footer pointing at arbitrary offsets.
    for off in [0u64, 1, 16, u64::MAX, bytes.len() as u64 - 21, bytes.len() as u64 - 52] {
        let mut data = bytes.clone();
        let n = data.len();
        data[n - 20..n - 12].copy_from_slice(&off.to_le_bytes());
        try_open(&data);
    }
}

#[test]
fn delta_incremental_chain() {
    let spec = ImageSpec { compress: true, password: Some(("pw", "aes-256")), ..sample_spec() };
    let dir = tempfile::tempdir().unwrap();
    let (full, full_index) = single_file(&spec);
    let full_path = write(dir.path(), "SET-00-00.mrimgx", &full);

    // Changes: block 5 of partition 1 rewritten, its hole (block 3) filled, block 2 of
    // partition 2 rewritten.
    let mut new_spec = spec.clone();
    let changes = [(0usize, 5usize), (0, 3), (1, 2)];
    for &(p, b) in &changes {
        let part = &mut new_spec.disks[0].parts[p];
        let at = part.data_base as usize + b * part.block_size as usize;
        part.content[at..at + 4096].copy_from_slice(&pattern(4096, 0x40 + b as u8));
        part.holes.retain(|&h| h != b);
    }
    let meta = FileMeta { file_number: 1, increment: 1, backup_type: "inc", split: false, delta: true };
    let mut w = FileWriter::new(&new_spec, meta);
    let mut index: Vec<Vec<PartIndex>> =
        vec![full_index[0].iter().map(|p| PartIndex { reserved: p.reserved.clone(), ..Default::default() }).collect()];
    for &(p, b) in &changes {
        let part = &new_spec.disks[0].parts[p];
        let at = part.data_base as usize + b * part.block_size as usize;
        let elem = w.add_block(&part.content[at..at + 4096], 0, part.number, b as u32);
        index[0][p].deltas.push((b as u32, elem));
    }
    let inc_path = write(dir.path(), "SET-00-01.mrimgx", &w.finish(Some(&index)));

    let img = MrimgxImage::open(&inc_path, Some("pw")).unwrap();
    assert_eq!(img.info().kind, "incremental");
    check_partitions(&img, &new_spec);
    let v = img.verify(&mut |_, _| true).unwrap();
    assert_eq!((v.ok, v.checked_blocks), (true, 17));

    // The full image alone still shows the old state.
    check_partitions(&MrimgxImage::open(&full_path, Some("pw")).unwrap(), &spec);

    std::fs::remove_file(&full_path).unwrap();
    assert!(matches!(MrimgxImage::open(&inc_path, Some("pw")), Err(Error::MissingFile(_))));
}
