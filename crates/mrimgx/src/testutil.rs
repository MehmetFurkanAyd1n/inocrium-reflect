//! Synthetic Reflect X image writer used by the tests (no real images are shipped).

use md5::{Digest, Md5};
use serde_json::{json, Value};

use crate::crypto::Crypto;
use crate::format::{names, BlockHeader, IndexElem, MAGIC};

pub(crate) const IMAGE_ID: [u8; 8] = [0x0A, 0xD2, 0xFD, 0x93, 0x62, 0xD4, 0x93, 0xB1];
pub(crate) const ITERATIONS: u32 = 16;

/// One partition to image. `content` is the full partition content.
#[derive(Clone)]
pub(crate) struct PartSpec {
    pub number: u16,
    pub start: u64,
    pub content: Vec<u8>,
    pub block_size: u32,
    /// Partition-relative offset of data block 0.
    pub data_base: u64,
    /// FAT reserved sectors: `(boot_sector_offset, byte length, reserved block size)`.
    pub reserved: Option<(u64, u64, usize)>,
    /// Data blocks that are not stored.
    pub holes: Vec<usize>,
    pub fs_type: &'static str,
    pub mbr_type: u8,
    pub gpt_type: [u8; 16],
}

impl PartSpec {
    pub fn new(number: u16, start: u64, content: Vec<u8>, block_size: u32) -> Self {
        Self {
            number,
            start,
            content,
            block_size,
            data_base: 0,
            reserved: None,
            holes: Vec::new(),
            fs_type: "NTFS",
            mbr_type: 0x07,
            gpt_type: [0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7],
        }
    }

    fn data_blocks(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let base = (self.data_base as usize).min(self.content.len());
        self.content[base..].chunks(self.block_size as usize).enumerate()
    }

    fn reserved_blocks(&self) -> Vec<&[u8]> {
        match self.reserved {
            Some((bso, len, chunk)) => self.content[bso as usize..(bso + len) as usize].chunks(chunk).collect(),
            None => Vec::new(),
        }
    }

    /// What a reader must return for this partition (holes and unstored areas are zeros).
    pub fn expected(&self) -> Vec<u8> {
        let mut out = vec![0; self.content.len()];
        if let Some((bso, len, _)) = self.reserved {
            let r = bso as usize..(bso + len) as usize;
            out[r.clone()].copy_from_slice(&self.content[r]);
        }
        let base = self.data_base as usize;
        for (i, b) in self.data_blocks() {
            if !self.holes.contains(&i) {
                let at = base + i * self.block_size as usize;
                out[at..at + b.len()].copy_from_slice(b);
            }
        }
        out
    }
}

/// One disk to image.
#[derive(Clone)]
pub(crate) struct DiskSpec {
    pub number: u16,
    pub sector_size: u32,
    pub size: u64,
    pub gpt: bool,
    pub signature: String,
    pub track0_len: usize,
    pub ept: Vec<(u64, [u8; 512])>,
    pub parts: Vec<PartSpec>,
}

impl DiskSpec {
    /// Track 0 with an MBR (or protective MBR + primary GPT) describing `parts`.
    pub fn track0(&self) -> Vec<u8> {
        let ss = self.sector_size as usize;
        let mut t = vec![0; self.track0_len];
        t[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        t[510] = 0x55;
        t[511] = 0xAA;
        let mbr_entry = |t: &mut [u8], i: usize, ty: u8, lba: u64, sectors: u64| {
            let e = &mut t[446 + i * 16..462 + i * 16];
            e[4] = ty;
            e[8..12].copy_from_slice(&(lba as u32).to_le_bytes());
            e[12..16].copy_from_slice(&(sectors.min(u32::MAX as u64) as u32).to_le_bytes());
        };
        if self.gpt {
            mbr_entry(&mut t, 0, 0xEE, 1, self.size / ss as u64 - 1);
            let h = &mut t[ss..ss + 92];
            h[..8].copy_from_slice(b"EFI PART");
            h[56..72].copy_from_slice(&[0x11; 16]);
            h[72..80].copy_from_slice(&2u64.to_le_bytes());
            h[80..84].copy_from_slice(&128u32.to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            for (i, p) in self.parts.iter().enumerate() {
                let e = &mut t[2 * ss + i * 128..2 * ss + (i + 1) * 128];
                e[..16].copy_from_slice(&p.gpt_type);
                e[16..32].copy_from_slice(&[i as u8 + 1; 16]);
                e[32..40].copy_from_slice(&(p.start / ss as u64).to_le_bytes());
                e[40..48].copy_from_slice(&((p.start + p.content.len() as u64) / ss as u64 - 1).to_le_bytes());
            }
        } else {
            for (i, p) in self.parts.iter().enumerate().take(4) {
                mbr_entry(&mut t, i, p.mbr_type, p.start / ss as u64, p.content.len() as u64 / ss as u64);
            }
        }
        t
    }
}

/// The image (backup definition) shared by all files of a set.
#[derive(Clone)]
pub(crate) struct ImageSpec {
    pub compress: bool,
    /// `(password, aes_type)`.
    pub password: Option<(&'static str, &'static str)>,
    pub disks: Vec<DiskSpec>,
}

/// Header fields of one file of a set.
#[derive(Clone, Copy)]
pub(crate) struct FileMeta {
    pub file_number: u16,
    pub increment: i64,
    pub backup_type: &'static str,
    pub split: bool,
    pub delta: bool,
}

impl FileMeta {
    pub const FULL: Self = Self { file_number: 0, increment: 0, backup_type: "full", split: false, delta: false };
}

/// Block index of one partition as written to `$INDEX`.
#[derive(Clone, Default)]
pub(crate) struct PartIndex {
    pub reserved: Vec<IndexElem>,
    pub blocks: Vec<IndexElem>,
    pub deltas: Vec<(u32, IndexElem)>,
}

/// Builds the bytes of one `.mrimgx` file.
pub(crate) struct FileWriter<'a> {
    spec: &'a ImageSpec,
    meta: FileMeta,
    crypto: Option<(Crypto, [u8; 32])>,
    pub buf: Vec<u8>,
}

impl<'a> FileWriter<'a> {
    pub fn new(spec: &'a ImageSpec, meta: FileMeta) -> Self {
        let crypto = spec.password.map(|(pw, aes)| {
            let key = Crypto::derive_key(pw, &IMAGE_ID, ITERATIONS);
            (Crypto::new(aes, &key, IMAGE_ID).unwrap(), key)
        });
        // Some leading bytes so that no structure starts at offset 0.
        Self { spec, meta, crypto, buf: b"mrimgx test file".to_vec() }
    }

    fn encrypts(&self) -> bool {
        self.spec.password.is_some_and(|(_, aes)| aes != "none")
    }

    /// Appends a data block and returns its index element.
    pub fn add_block(&mut self, data: &[u8], disk: u16, part: u16, iv_index: u32) -> IndexElem {
        let mut stored = if self.spec.compress { zstd::bulk::compress(data, 3).unwrap() } else { data.to_vec() };
        let mut decoded = data.to_vec();
        if self.encrypts() {
            let pad = stored.len().next_multiple_of(16);
            stored.resize(pad, 0);
            if !self.spec.compress {
                // Without compression the reader sees the padding as part of the block.
                decoded = stored.clone();
            }
            let (c, _) = self.crypto.as_ref().unwrap();
            c.encrypt_cbc(&c.iv(disk, part, iv_index), &mut stored);
        }
        let elem = IndexElem {
            pos: self.buf.len() as u64,
            md5: Md5::digest(&decoded).into(),
            len: stored.len() as u32,
            file: self.meta.file_number,
        };
        self.buf.extend_from_slice(&stored);
        elem
    }

    /// Appends a metadata block list.
    fn add_list(&mut self, blocks: &[(&[u8; 8], Vec<u8>, bool)]) {
        for (i, (name, payload, may_encrypt)) in blocks.iter().enumerate() {
            let mut data = if self.spec.compress { zstd::bulk::compress(payload, 3).unwrap() } else { payload.clone() };
            let encrypt = *may_encrypt && self.encrypts();
            if encrypt {
                data.resize(data.len().next_multiple_of(16), 0);
                self.crypto.as_ref().unwrap().0.encrypt_ecb(&mut data);
            }
            let hdr = BlockHeader::new(
                name,
                data.len() as u32,
                Md5::digest(&data).into(),
                i + 1 == blocks.len(),
                self.spec.compress,
                encrypt,
            );
            self.buf.extend_from_slice(&hdr.to_bytes());
            self.buf.extend_from_slice(&data);
        }
    }

    /// Writes the metadata (disk/partition lists when `index` is given, `$JSON`) and footer.
    pub fn finish(mut self, index: Option<&[Vec<PartIndex>]>) -> Vec<u8> {
        let index_pos = self.buf.len() as u64;
        if let Some(index) = index {
            for (disk, parts) in self.spec.disks.iter().zip(index) {
                let mut list = vec![(names::TRACK0, disk.track0(), true)];
                if !disk.ept.is_empty() {
                    let mut ept = (disk.ept.len() as u32).to_le_bytes().to_vec();
                    for (off, rec) in &disk.ept {
                        ept.extend_from_slice(rec);
                        ept.extend_from_slice(&off.to_le_bytes());
                        ept.extend_from_slice(&[0; 2 + 6]); // partition number + MSVC padding
                    }
                    list.push((names::EPT, ept, true));
                }
                self.add_list(&list);
                for p in parts {
                    let mut data = Vec::new();
                    data.extend_from_slice(&(p.reserved.len() as i32).to_le_bytes());
                    p.reserved.iter().for_each(|e| e.write(&mut data));
                    if self.meta.delta {
                        data.extend_from_slice(&(p.deltas.len() as i32).to_le_bytes());
                        for (i, e) in &p.deltas {
                            e.write(&mut data);
                            data.extend_from_slice(&i.to_le_bytes());
                        }
                    } else {
                        data.extend_from_slice(&(p.blocks.len() as i32).to_le_bytes());
                        p.blocks.iter().for_each(|e| e.write(&mut data));
                    }
                    self.add_list(&[(names::INDEX, data, true)]);
                }
            }
        }
        let json = self.json(index_pos).to_string().into_bytes();
        let first = self.buf.len() as u64;
        self.add_list(&[(names::JSON, json, false)]);
        self.buf.extend_from_slice(&first.to_le_bytes());
        self.buf.extend_from_slice(MAGIC);
        self.buf
    }

    fn json(&self, index_pos: u64) -> Value {
        let m = self.meta;
        let encryption = match (&self.spec.password, &self.crypto) {
            (Some((_, aes)), Some((_, key))) => json!({
                "enable": true,
                "aes_type": aes,
                "key_iterations": ITERATIONS,
                "hmac": hex(&Crypto::key_hmac(key)),
            }),
            _ => json!({ "enable": false }),
        };
        let disks: Vec<Value> = self
            .spec
            .disks
            .iter()
            .map(|d| {
                let parts: Vec<Value> = d
                    .parts
                    .iter()
                    .map(|p| {
                        let len = p.content.len() as u64;
                        let (bso, rlen) = p.reserved.map_or((0, 0), |(b, l, _)| (b, l));
                        // The file system start is recorded relative to nothing in particular; use
                        // an arbitrary base to check that only the difference matters.
                        let fs_start = 1_000_000 + p.start;
                        json!({
                            "_header": {
                                "block_size": p.block_size,
                                "block_count": p.data_blocks().count(),
                                "partition_number": p.number,
                            },
                            "_geometry": { "start": p.start, "end": p.start + len - 1, "length": len, "boot_sector_offset": bso },
                            "_partition_table_entry": { "type": p.mbr_type },
                            "_file_system": {
                                "type": p.fs_type,
                                "drive_letter": 67,
                                "start": fs_start,
                                "lcn0_offset": fs_start + p.data_base,
                                "reserved_sectors_byte_length": rlen,
                                "volume_label": format!("VOL{}", p.number),
                                "total_clusters": 100,
                                "free_clusters": 40,
                                "sectors_per_cluster": 8,
                            },
                        })
                    })
                    .collect();
                json!({
                    "_header": {
                        "disk_format": if d.gpt { "gpt" } else { "mbr" },
                        "disk_number": d.number,
                        "disk_signature": d.signature,
                    },
                    "_geometry": { "bytes_per_sector": d.sector_size, "disk_size": d.size },
                    "partitions": parts,
                })
            })
            .collect();
        json!({
            "_header": {
                "imageid": hex(&IMAGE_ID),
                "file_number": m.file_number,
                "increment_number": m.increment,
                "merged_files": [],
                "split_file": m.split,
                "netbios_name": "TESTPC",
                "backup_time": 1_700_000_000 + m.increment,
                "index_file_position": index_pos,
                "delta_index": m.delta,
                "backup_type": m.backup_type,
            },
            "_encryption": encryption,
            "_compression": { "compression_level": if self.spec.compress { "medium" } else { "none" } },
            "_auxiliary_data": { "backup_definition": { "comment": "synthetic test image" } },
            "disks": disks,
        })
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Stores every reserved and data block of `spec` (skipping holes); `pick(n)` chooses the
/// writer for the `n`-th stored block. Returns the full block index.
pub(crate) fn store_all(
    spec: &ImageSpec,
    writers: &mut [FileWriter<'_>],
    mut pick: impl FnMut(usize) -> usize,
) -> Vec<Vec<PartIndex>> {
    let mut n = 0;
    let mut next = |data: &[u8], disk: u16, part: u16, i: usize| {
        let w = pick(n);
        n += 1;
        writers[w].add_block(data, disk, part, i as u32)
    };
    spec.disks
        .iter()
        .map(|d| {
            d.parts
                .iter()
                .map(|p| PartIndex {
                    reserved: p
                        .reserved_blocks()
                        .iter()
                        .enumerate()
                        .map(|(i, b)| next(b, d.number, p.number, i))
                        .collect(),
                    blocks: p
                        .data_blocks()
                        .map(
                            |(i, b)| {
                                if p.holes.contains(&i) {
                                    IndexElem::default()
                                } else {
                                    next(b, d.number, p.number, i)
                                }
                            },
                        )
                        .collect(),
                    deltas: Vec::new(),
                })
                .collect()
        })
        .collect()
}

/// Writes `spec` as a single full image file.
pub(crate) fn single_file(spec: &ImageSpec) -> (Vec<u8>, Vec<Vec<PartIndex>>) {
    let mut w = [FileWriter::new(spec, FileMeta::FULL)];
    let index = store_all(spec, &mut w, |_| 0);
    let [w] = w;
    (w.finish(Some(&index)), index)
}

/// Deterministic pseudo-random bytes (compressible enough for zstd, distinct per seed).
pub(crate) fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i / 7) as u8).wrapping_mul(31).wrapping_add(seed) ^ (i as u8 & 0x0F)).collect()
}

/// A GPT disk with two partitions (the second FAT-like with reserved sectors and a hole) and a
/// 512-byte sector size.
pub(crate) fn sample_spec() -> ImageSpec {
    let mut p1 = PartSpec::new(1, 64 * 1024, pattern(40 * 1024, 1), 4096);
    p1.holes = vec![3];
    let mut p2 = PartSpec::new(2, 128 * 1024, pattern(24 * 1024, 2), 4096);
    p2.fs_type = "FAT32";
    p2.reserved = Some((0, 3072, 1024));
    p2.data_base = 4096;
    p2.holes = vec![1];
    ImageSpec {
        compress: false,
        password: None,
        disks: vec![DiskSpec {
            number: 0,
            sector_size: 512,
            size: 256 * 1024,
            gpt: true,
            signature: "6A578E4D-7B5C-4CC1-85D3-FFCA233A9B93".into(),
            track0_len: 32 * 1024,
            ept: Vec::new(),
            parts: vec![p1, p2],
        }],
    }
}
