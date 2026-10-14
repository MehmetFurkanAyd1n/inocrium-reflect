use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use irimg::*;
use uuid::Uuid;

const BS: u32 = MIN_BLOCK_SIZE;

fn manifest(id: Uuid) -> Manifest {
    Manifest {
        format_version: FORMAT_VERSION,
        image_id: id,
        created: "2026-01-01T00:00:00Z".into(),
        comment: "test image".into(),
        tool_version: "test".into(),
        source: SourceInfo::default(),
        block_size: BS,
        compression: CompressionInfo { algorithm: "zstd".into(), level: 3 },
        encryption: None,
        definition_id: None,
        disks: vec![DiskManifest {
            index: 0,
            source_number: 1,
            model: "Test".into(),
            serial: String::new(),
            size: 64 << 20,
            sector_size: 512,
            physical_sector_size: 4096,
            layout: PartitionStyle::Raw,
            partitions: vec![],
            gaps: vec![],
            raw_regions: vec![RawRegionInfo { offset: 0, length: 4096, kind: RawRegionKind::Track0 }],
        }],
    }
}

/// Deterministic, partly compressible block content.
fn block_data(stream: u32, block: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let mut x = (stream as u64) << 32 ^ block ^ 0x9E37_79B9_7F4A_7C15;
    for (i, b) in v.iter_mut().enumerate() {
        if i % 4096 < 2048 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        } else {
            *b = (i / 4096) as u8;
        }
    }
    v
}

fn opts(password: Option<&str>, split: Option<u64>) -> WriterOptions {
    WriterOptions {
        block_size: BS,
        compression_level: 3,
        password: password.map(Into::into),
        kdf: KdfParams::insecure_fast(),
        split_size: split,
    }
}

fn write_image(path: &std::path::Path, o: WriterOptions, blocks: &[(u32, u64)]) -> (Uuid, FinishSummary) {
    let mut w = ImageWriter::create(path, o).unwrap();
    let enc = w.encoder();
    for &(s, b) in blocks {
        let len = if b == 9 { 1000 } else { BS as usize };
        w.append(enc.encode(BlockRef { disk: 0, stream: s, block: b }, &block_data(s, b, len))).unwrap();
    }
    let id = w.image_id();
    let raw = vec![RawRegion { disk: 0, offset: 0, data: vec![0xAB; 4096] }];
    let sum = w.finish(&manifest(id), &raw).unwrap();
    (id, sum)
}

#[test]
fn roundtrip_plain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("img.irimg");
    let blocks = [(1, 0), (1, 1), (1, 5), (2, 0), (1, 9)];
    let (id, sum) = write_image(&path, opts(None, None), &blocks);
    assert_eq!(sum.files.len(), 1);
    let r = ImageReader::open(&path, None).unwrap();
    assert_eq!(r.image_id(), id);
    assert_eq!(r.manifest().comment, "test image");
    assert_eq!(r.raw_regions(0).next().unwrap().data, vec![0xAB; 4096]);
    for &(s, b) in &blocks {
        let len = if b == 9 { 1000 } else { BS as usize };
        assert_eq!(r.read_block(0, s, b).unwrap().unwrap(), block_data(s, b, len));
    }
    assert!(r.read_block(0, 1, 2).unwrap().is_none());
    // Stream reads across a block boundary, through a hole.
    let mut buf = vec![0u8; BS as usize * 2];
    r.read_stream_at(0, 1, BS as u64 / 2, &mut buf).unwrap();
    assert_eq!(&buf[..BS as usize / 2], &block_data(1, 0, BS as usize)[BS as usize / 2..]);
    assert_eq!(&buf[BS as usize / 2..BS as usize * 3 / 2], &block_data(1, 1, BS as usize)[..]);
    assert!(buf[BS as usize * 3 / 2..].iter().all(|&b| b == 0));
    let rep = r.verify(|_| true).unwrap();
    assert!(rep.ok());
    assert_eq!(rep.blocks_checked, blocks.len() as u64);
}

#[test]
fn encrypted_and_wrong_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc.irimg");
    write_image(&path, opts(Some("pässword"), None), &[(1, 0), (1, 1)]);
    assert!(ImageReader::peek_header(&path).unwrap().encrypted());
    assert!(matches!(ImageReader::open(&path, None), Err(Error::PasswordRequired)));
    assert!(matches!(ImageReader::open(&path, Some("password")), Err(Error::WrongPassword)));
    let r = ImageReader::open(&path, Some("pässword")).unwrap();
    assert_eq!(r.read_block(0, 1, 1).unwrap().unwrap(), block_data(1, 1, BS as usize));
    // Data must not appear in clear text.
    let raw = std::fs::read(&path).unwrap();
    let needle = &block_data(1, 1, BS as usize)[..64];
    assert!(!raw.windows(64).any(|w| w == needle));
}

fn corrupt_byte(path: &std::path::Path, offset: u64) {
    let mut f = OpenOptions::new().read(true).write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8];
    f.read_exact(&mut b).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[b[0] ^ 0x55]).unwrap();
}

#[test]
fn corruption_detected_plain_and_encrypted() {
    for pw in [None, Some("x")] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.irimg");
        write_image(&path, opts(pw, None), &[(1, 0), (1, 1), (1, 2)]);
        let r = ImageReader::open(&path, pw).unwrap();
        let e = r.stream_entries(0, 1)[1];
        drop(r);
        corrupt_byte(&path, e.offset + e.stored_len as u64 / 2);
        let r = ImageReader::open(&path, pw).unwrap();
        assert!(matches!(r.read_block(0, 1, 1), Err(Error::CorruptBlock { block: 1, .. })));
        assert!(r.read_block(0, 1, 0).is_ok());
        let rep = r.verify(|_| true).unwrap();
        assert_eq!(rep.bad_blocks.len(), 1);
        assert_eq!(rep.bad_blocks[0].block, 1);
    }
}

#[test]
fn swapped_blocks_detected() {
    for pw in [None, Some("x")] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.irimg");
        // Two blocks with identical stored length so they can be swapped in place.
        let mut w = ImageWriter::create(&path, WriterOptions { compression_level: 0, ..opts(pw, None) }).unwrap();
        let enc = w.encoder();
        w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: 0 }, &vec![1u8; BS as usize])).unwrap();
        w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: 1 }, &vec![2u8; BS as usize])).unwrap();
        let id = w.image_id();
        w.finish(&manifest(id), &[]).unwrap();

        let r = ImageReader::open(&path, pw).unwrap();
        let (a, b) = (r.stream_entries(0, 1)[0], r.stream_entries(0, 1)[1]);
        drop(r);
        let mut data = std::fs::read(&path).unwrap();
        let (ao, bo, l) = (a.offset as usize, b.offset as usize, a.stored_len as usize);
        let tmp = data[ao..ao + l].to_vec();
        data.copy_within(bo..bo + l, ao);
        data[bo..bo + l].copy_from_slice(&tmp);
        std::fs::write(&path, data).unwrap();
        let r = ImageReader::open(&path, pw).unwrap();
        assert!(r.read_block(0, 1, 0).is_err());
        assert!(r.read_block(0, 1, 1).is_err());
    }
}

#[test]
fn split_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sp.irimg");
    let blocks: Vec<(u32, u64)> = (0..12).map(|b| (1, b)).collect();
    let split = HEADER_SIZE as u64 + BS as u64 * 2;
    let (_, sum) = write_image(&path, opts(Some("pw"), Some(split)), &blocks);
    assert!(sum.files.len() > 2, "expected several parts, got {}", sum.files.len());
    assert!(sum.files[1].to_string_lossy().ends_with(".i01"));
    assert_eq!(image_files(&path).len(), sum.files.len());
    // Opening via any part works.
    let r = ImageReader::open(&sum.files[1], Some("pw")).unwrap();
    for &(s, b) in &blocks {
        let len = if b == 9 { 1000 } else { BS as usize };
        assert_eq!(r.read_block(0, s, b).unwrap().unwrap(), block_data(s, b, len));
    }
    assert!(r.verify(|_| true).unwrap().ok());
    // A missing part is reported.
    std::fs::remove_file(&sum.files[1]).unwrap();
    assert!(ImageReader::open(&path, Some("pw")).is_err());
}

#[test]
fn incomplete_image_has_no_footer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i.irimg");
    let mut w = ImageWriter::create(&path, opts(None, None)).unwrap();
    let enc = w.encoder();
    w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: 0 }, &[7u8; 100])).unwrap();
    drop(w);
    assert!(matches!(ImageReader::open(&path, None), Err(Error::CorruptFooter)));
}

#[test]
fn checkpoint_resume() {
    for pw in [None, Some("pw")] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.irimg");
        let split = Some(HEADER_SIZE as u64 + BS as u64 * 3);
        let mut w = ImageWriter::create(&path, opts(pw, split)).unwrap();
        let enc = w.encoder();
        for b in 0..5 {
            w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: b }, &block_data(1, b, BS as usize))).unwrap();
        }
        let ckpt = w.checkpoint(serde_json::json!({ "next_block": 5 })).unwrap();
        // Blocks written after the checkpoint, then a crash.
        for b in 5..9 {
            w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: b }, &[0xEE; 4096])).unwrap();
        }
        drop(w);
        let ckpt: Checkpoint = serde_json::from_str(&serde_json::to_string(&ckpt).unwrap()).unwrap();

        if pw.is_some() {
            assert!(matches!(ImageWriter::resume(&path, &ckpt, Some("nope")), Err(Error::WrongPassword)));
        }
        let mut w = ImageWriter::resume(&path, &ckpt, pw).unwrap();
        assert_eq!(ckpt.user["next_block"], 5);
        assert!(w.contains(0, 1, 4) && !w.contains(0, 1, 5));
        let enc = w.encoder();
        for b in 5..10 {
            w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: b }, &block_data(1, b, BS as usize))).unwrap();
        }
        let id = w.image_id();
        w.finish(&manifest(id), &[]).unwrap();

        let r = ImageReader::open(&path, pw).unwrap();
        for b in 0..10 {
            assert_eq!(r.read_block(0, 1, b).unwrap().unwrap(), block_data(1, b, BS as usize), "block {b}");
        }
        assert!(r.verify(|_| true).unwrap().ok());
        let nonces: std::collections::HashSet<u64> = r.stream_entries(0, 1).iter().map(|e| e.nonce).collect();
        assert_eq!(nonces.len(), 10);
    }
}
