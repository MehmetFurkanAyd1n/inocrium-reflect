//! Read-only block devices backed by an `.irimg` image.

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;

use irimg::{DiskManifest, ImageReader, PartitionManifest};
use lru::LruCache;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::device::{check_range, write_protected, BlockDevice};
use crate::gpt::{self, Gpt, GptEntry};
use crate::table;

const MIB: u64 = 1 << 20;

#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Present the disk with a fresh MBR signature / GPT disk and partition GUIDs so that it
    /// does not collide with the (possibly still attached) source disk.
    pub rewrite_identity: bool,
    /// Number of decoded image blocks kept in memory.
    pub cache_blocks: usize,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self { rewrite_identity: true, cache_blocks: 64 }
    }
}

#[derive(Clone)]
enum Source {
    Zero,
    /// Disk offset `start` of the segment maps to `buf[base]`.
    Bytes {
        buf: Arc<Vec<u8>>,
        base: usize,
    },
    /// Disk offset `start` of the segment maps to stream offset `base`.
    Stream {
        stream: u32,
        base: u64,
    },
}

impl Source {
    fn advanced(&self, by: u64) -> Source {
        match self {
            Source::Zero => Source::Zero,
            Source::Bytes { buf, base } => Source::Bytes { buf: buf.clone(), base: base + by as usize },
            Source::Stream { stream, base } => Source::Stream { stream: *stream, base: base + by },
        }
    }
}

#[derive(Clone)]
struct Segment {
    start: u64,
    end: u64,
    src: Source,
}

/// Sorted, non-overlapping segments covering `[0, size)`.
struct SegmentMap(Vec<Segment>);

impl SegmentMap {
    fn new(size: u64) -> Self {
        Self(vec![Segment { start: 0, end: size, src: Source::Zero }])
    }

    /// Makes `[start, start + len)` (clipped to the map) come from `src`.
    fn paint(&mut self, start: u64, len: u64, src: Source) {
        let size = self.0.last().map_or(0, |s| s.end);
        let end = start.saturating_add(len).min(size);
        if start >= end {
            return;
        }
        let mut out = Vec::with_capacity(self.0.len() + 2);
        for s in self.0.drain(..) {
            if s.end <= start || s.start >= end {
                out.push(s);
                continue;
            }
            if s.start < start {
                out.push(Segment { start: s.start, end: start, src: s.src.clone() });
            }
            if s.end > end {
                out.push(Segment { start: end, end: s.end, src: s.src.advanced(end - s.start) });
            }
        }
        out.push(Segment { start, end, src });
        out.sort_by_key(|s| s.start);
        self.0 = out;
    }

    fn find(&self, offset: u64) -> usize {
        self.0.partition_point(|s| s.end <= offset)
    }
}

type BlockCache = LruCache<(u32, u64), Option<Arc<Vec<u8>>>>;

/// A disk (or a single partition presented as a disk) reconstructed from an image.
///
/// Unstored blocks and uncaptured partitions read as zeros. The partition table is
/// regenerated so it only lists captured partitions, optionally with new identities.
pub struct ImageDiskView {
    reader: Arc<ImageReader>,
    disk: u32,
    size: u64,
    sector_size: u32,
    image_block: u64,
    segments: SegmentMap,
    cache: Mutex<BlockCache>,
    partitions: Vec<(u32, u64)>,
}

impl ImageDiskView {
    /// The whole disk `disk` of the image.
    pub fn disk(reader: Arc<ImageReader>, disk: u32, opts: ViewOptions) -> io::Result<Self> {
        let d = disk_manifest(&reader, disk)?.clone();
        if !table::valid_gpt_geometry(&d) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported GPT geometry in image"));
        }
        let mut segments = SegmentMap::new(d.size);
        let mut track0 = None;
        for r in reader.raw_regions(disk) {
            if r.offset == 0 && r.data.len() >= 512 {
                track0 = Some(r.data[..512].to_vec());
            }
            segments.paint(r.offset, r.data.len() as u64, bytes(r.data.clone()));
        }
        let captured: Vec<&PartitionManifest> = d.captured_partitions().collect();
        for p in &captured {
            segments.paint(p.offset, p.length, Source::Stream { stream: p.stream, base: 0 });
        }
        for g in &d.gaps {
            segments.paint(g.offset, g.length, Source::Stream { stream: g.stream, base: 0 });
        }
        for (offset, data) in table::disk_table(&d, track0.as_deref(), d.size, opts.rewrite_identity) {
            segments.paint(offset, data.len() as u64, bytes(data));
        }
        Ok(Self::new(reader, &d, d.size, segments, captured.iter().map(|p| (p.number, p.offset)).collect(), &opts))
    }

    /// A single captured partition, wrapped in a synthetic GPT disk (1 MiB head, the partition
    /// as a basic data partition, 1 MiB tail) so Windows can mount it on its own.
    pub fn partition(reader: Arc<ImageReader>, disk: u32, number: u32, opts: ViewOptions) -> io::Result<Self> {
        let d = disk_manifest(&reader, disk)?.clone();
        let p = d
            .partition(number)
            .filter(|p| p.captured)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("partition {number} not in image")))?;
        if !table::valid_sector_size(d.sector_size) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid sector size in image"));
        }
        let ss = d.sector_size as u64;
        let size = p
            .length
            .div_ceil(MIB)
            .checked_mul(MIB)
            .and_then(|n| n.checked_add(2 * MIB))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "partition too large"))?;
        let table = Gpt {
            disk_id: Uuid::new_v4(),
            sector_size: d.sector_size,
            entry_count: gpt::DEFAULT_ENTRY_COUNT,
            entries: vec![GptEntry {
                type_id: gpt::BASIC_DATA,
                id: Uuid::new_v4(),
                first_lba: MIB / ss,
                last_lba: (MIB + p.length) / ss - 1,
                attributes: 0,
                name: if p.label.is_empty() { "inocrium".into() } else { p.label.clone() },
            }],
        };
        let sectors = table.build(size, Some(MIB / ss), None);
        let mut segments = SegmentMap::new(size);
        segments.paint(0, sectors.primary.len() as u64, bytes(sectors.primary));
        segments.paint(MIB, p.length, Source::Stream { stream: p.stream, base: 0 });
        segments.paint(sectors.backup_offset, sectors.backup.len() as u64, bytes(sectors.backup));
        Ok(Self::new(reader, &d, size, segments, vec![(number, MIB)], &opts))
    }

    fn new(
        reader: Arc<ImageReader>,
        d: &DiskManifest,
        size: u64,
        segments: SegmentMap,
        partitions: Vec<(u32, u64)>,
        opts: &ViewOptions,
    ) -> Self {
        let image_block = reader.block_size() as u64;
        let cache = LruCache::new(NonZeroUsize::new(opts.cache_blocks.max(1)).unwrap());
        Self {
            reader,
            disk: d.index,
            size,
            sector_size: d.sector_size,
            image_block,
            segments,
            cache: Mutex::new(cache),
            partitions,
        }
    }

    /// Byte offset of an image partition within this view.
    pub fn partition_offset(&self, number: u32) -> Option<u64> {
        self.partitions.iter().find(|p| p.0 == number).map(|p| p.1)
    }

    fn block(&self, stream: u32, block: u64) -> io::Result<Option<Arc<Vec<u8>>>> {
        if let Some(b) = self.cache.lock().get(&(stream, block)) {
            return Ok(b.clone());
        }
        // Decode outside the lock; a concurrent miss on the same block just decodes twice.
        let data = self.reader.read_block(self.disk, stream, block).map_err(Into::<io::Error>::into)?.map(Arc::new);
        self.cache.lock().put((stream, block), data.clone());
        Ok(data)
    }

    fn read_stream(&self, stream: u32, mut pos: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let within = (pos % self.image_block) as usize;
            let n = (self.image_block as usize - within).min(buf.len());
            let (dst, rest) = buf.split_at_mut(n);
            match self.block(stream, pos / self.image_block)? {
                // A short (final) block may end before `within`.
                Some(data) if within < data.len() => {
                    let avail = (data.len() - within).min(n);
                    dst[..avail].copy_from_slice(&data[within..within + avail]);
                    dst[avail..].fill(0);
                }
                _ => dst.fill(0),
            }
            pos += n as u64;
            buf = rest;
        }
        Ok(())
    }
}

impl BlockDevice for ImageDiskView {
    fn size(&self) -> u64 {
        self.size
    }
    fn block_size(&self) -> u32 {
        self.sector_size
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        let mut i = self.segments.find(offset);
        while !buf.is_empty() {
            let seg = &self.segments.0[i];
            let n = ((seg.end - offset) as usize).min(buf.len());
            let (dst, rest) = buf.split_at_mut(n);
            let rel = offset - seg.start;
            match &seg.src {
                Source::Zero => dst.fill(0),
                Source::Bytes { buf, base } => {
                    let from = base + rel as usize;
                    dst.copy_from_slice(&buf[from..from + n]);
                }
                Source::Stream { stream, base } => self.read_stream(*stream, base + rel, dst)?,
            }
            offset += n as u64;
            buf = rest;
            i += 1;
        }
        Ok(())
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(write_protected())
    }
}

fn disk_manifest(reader: &ImageReader, disk: u32) -> io::Result<&DiskManifest> {
    reader
        .manifest()
        .disk(disk)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("disk {disk} not in image")))
}

fn bytes(buf: Vec<u8>) -> Source {
    Source::Bytes { buf: Arc::new(buf), base: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mbr;
    use irimg::*;

    const BS: u32 = MIN_BLOCK_SIZE;
    const SIZE: u64 = 16 << 20;

    fn pattern(stream: u32, block: u64) -> Vec<u8> {
        (0..BS as usize).map(|i| (i as u64 ^ (block * 31) ^ (stream as u64 * 7)) as u8).collect()
    }

    fn part(number: u32, stream: u32, offset: u64, length: u64, captured: bool, gpt: bool) -> PartitionManifest {
        PartitionManifest {
            number,
            stream,
            offset,
            length,
            captured,
            capture: CaptureMode::Smart,
            gpt: gpt.then(|| GptPartitionInfo {
                type_id: crate::gpt::BASIC_DATA,
                id: Uuid::from_u128(number as u128),
                name: format!("p{number}"),
                attributes: 0,
            }),
            mbr: (!gpt).then_some(MbrPartitionInfo { partition_type: 0x07, bootable: number == 1, logical: false }),
            filesystem: "NTFS".into(),
            label: format!("L{number}"),
            drive_letter: None,
            used_bytes: None,
            stored_bytes: 0,
            is_system: false,
            is_boot: false,
        }
    }

    /// Two partitions at 1 MiB (4 MiB, captured, blocks 0..3 stored except block 2) and
    /// 6 MiB (2 MiB, not captured).
    fn image(dir: &std::path::Path, layout: PartitionStyle, track0: Vec<u8>) -> Arc<ImageReader> {
        image_with(dir, layout, track0, &[(0, BS as usize), (1, BS as usize), (3, BS as usize)], 512)
    }

    /// Like [`image`], storing the first `len` bytes of each `(block, len)` of stream 1.
    fn image_with(
        dir: &std::path::Path,
        layout: PartitionStyle,
        track0: Vec<u8>,
        blocks: &[(u64, usize)],
        sector_size: u32,
    ) -> Arc<ImageReader> {
        let gpt = matches!(layout, PartitionStyle::Gpt { .. });
        let path = dir.join("t.irimg");
        let mut w = ImageWriter::create(&path, WriterOptions { block_size: BS, ..Default::default() }).unwrap();
        let enc = w.encoder();
        for &(b, len) in blocks {
            w.append(enc.encode(BlockRef { disk: 0, stream: 1, block: b }, &pattern(1, b)[..len])).unwrap();
        }
        let m = Manifest {
            format_version: FORMAT_VERSION,
            image_id: w.image_id(),
            created: String::new(),
            comment: String::new(),
            tool_version: String::new(),
            source: SourceInfo::default(),
            block_size: BS,
            compression: CompressionInfo { algorithm: "zstd".into(), level: 3 },
            encryption: None,
            definition_id: None,
            disks: vec![DiskManifest {
                index: 0,
                source_number: 3,
                model: String::new(),
                serial: String::new(),
                size: SIZE,
                sector_size,
                physical_sector_size: 512,
                layout,
                partitions: vec![part(1, 1, MIB, 4 * MIB, true, gpt), part(2, 2, 6 * MIB, 2 * MIB, false, gpt)],
                gaps: vec![],
                raw_regions: vec![RawRegionInfo {
                    offset: 0,
                    length: track0.len() as u64,
                    kind: RawRegionKind::Track0,
                }],
            }],
        };
        w.finish(&m, &[RawRegion { disk: 0, offset: 0, data: track0 }]).unwrap();
        Arc::new(ImageReader::open(&path, None).unwrap())
    }

    fn read(dev: &dyn BlockDevice, offset: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        dev.read_at(offset, &mut b).unwrap();
        b
    }

    fn check_partition_data(dev: &dyn BlockDevice, at: u64) {
        let bs = BS as u64;
        assert_eq!(read(dev, at, BS as usize), pattern(1, 0));
        assert_eq!(read(dev, at + 3 * bs, BS as usize), pattern(1, 3));
        assert!(read(dev, at + 2 * bs, BS as usize).iter().all(|&b| b == 0));
        // Unaligned read spanning a stored and an unstored block.
        let r = read(dev, at + 2 * bs - 100, 200);
        assert_eq!(r[..100], pattern(1, 1)[BS as usize - 100..]);
        assert!(r[100..].iter().all(|&b| b == 0));
    }

    #[test]
    fn segment_map_paint() {
        let mut m = SegmentMap::new(100);
        m.paint(10, 50, Source::Stream { stream: 1, base: 0 });
        m.paint(20, 10, Source::Zero);
        m.paint(90, 50, Source::Zero);
        let spans: Vec<_> = m.0.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, [(0, 10), (10, 20), (20, 30), (30, 60), (60, 90), (90, 100)]);
        assert!(matches!(m.0[3].src, Source::Stream { stream: 1, base: 20 }));
        assert_eq!(m.find(0), 0);
        assert_eq!(m.find(59), 3);
        assert_eq!(m.find(60), 4);
    }

    #[test]
    fn gpt_disk_view() {
        let dir = tempfile::tempdir().unwrap();
        let original = Gpt {
            disk_id: Uuid::from_u128(42),
            sector_size: 512,
            entry_count: 128,
            entries: vec![
                GptEntry {
                    type_id: crate::gpt::BASIC_DATA,
                    id: Uuid::from_u128(1),
                    first_lba: 2048,
                    last_lba: 10239,
                    attributes: 0,
                    name: "p1".into(),
                },
                GptEntry {
                    type_id: crate::gpt::BASIC_DATA,
                    id: Uuid::from_u128(2),
                    first_lba: 12288,
                    last_lba: 16383,
                    attributes: 0,
                    name: "p2".into(),
                },
            ],
        };
        let mut track0 = original.build(SIZE, Some(2048), None).primary;
        track0[100] = 0x5A; // boot code byte in the protective MBR is preserved
        track0[(34 * 512) + 5] = 0x77; // and so is data after the entry array
        let layout = PartitionStyle::Gpt {
            disk_id: original.disk_id,
            first_usable_lba: 2048,
            last_usable_lba: SIZE / 512 - 34,
            max_partition_count: 128,
        };
        let reader = image(dir.path(), layout, track0);

        for rewrite in [false, true] {
            let v = ImageDiskView::disk(reader.clone(), 0, ViewOptions { rewrite_identity: rewrite, cache_blocks: 2 })
                .unwrap();
            assert_eq!((v.size(), v.block_size(), v.read_only()), (SIZE, 512, true));
            let head = read(&v, 0, MIB as usize);
            assert_eq!(head[100], 0x5A);
            assert_eq!(head[34 * 512 + 5], 0x77);
            let (h, e) = crate::gpt::parse_primary(&head, 512).unwrap();
            assert_eq!(e.len(), 1, "uncaptured partition removed");
            assert_eq!((e[0].first_lba, e[0].last_lba), (2048, 10239));
            assert_eq!(h.disk_id == original.disk_id, !rewrite);
            assert_eq!(e[0].id == Uuid::from_u128(1), !rewrite);
            let tail = read(&v, SIZE - 512, 512);
            let backup = crate::gpt::GptHeader::decode(&tail).unwrap();
            assert_eq!((backup.my_lba, backup.disk_id), (SIZE / 512 - 1, h.disk_id));
            check_partition_data(&v, MIB);
            assert!(read(&v, 6 * MIB, 4096).iter().all(|&b| b == 0));
            assert_eq!(v.partition_offset(1), Some(MIB));
            assert!(v.write_at(0, &[0; 512]).is_err());
            assert!(v.read_at(SIZE - 511, &mut [0; 512]).is_err());
        }
    }

    #[test]
    fn mbr_disk_view() {
        let dir = tempfile::tempdir().unwrap();
        let entries = [
            mbr::MbrEntry { bootable: true, partition_type: 0x07, first_lba: 2048, sectors: 8192 },
            mbr::MbrEntry { bootable: false, partition_type: 0x07, first_lba: 12288, sectors: 4096 },
            mbr::MbrEntry { bootable: false, partition_type: 0x0F, first_lba: 16384, sectors: 2048 },
        ];
        let mut track0 = vec![0u8; MIB as usize];
        track0[..512].copy_from_slice(&mbr::build(0x1234_5678, &entries, Some(&[0x90; 440])));
        let reader = image(dir.path(), PartitionStyle::Mbr { signature: 0x1234_5678 }, track0);

        let v = ImageDiskView::disk(reader.clone(), 0, ViewOptions { rewrite_identity: false, ..Default::default() })
            .unwrap();
        let s0 = read(&v, 0, 512);
        assert_eq!(mbr::signature(&s0), 0x1234_5678);
        assert_eq!(s0[0], 0x90);
        assert_eq!(mbr::entries(&s0), [Some(entries[0]), None, Some(entries[2]), None]);
        check_partition_data(&v, MIB);

        let v = ImageDiskView::disk(reader, 0, ViewOptions::default()).unwrap();
        let s0 = read(&v, 0, 512);
        assert_ne!(mbr::signature(&s0), 0x1234_5678);
        assert!(mbr::has_boot_signature(&s0));
    }

    #[test]
    fn partition_view() {
        let dir = tempfile::tempdir().unwrap();
        let reader = image(dir.path(), PartitionStyle::Mbr { signature: 1 }, vec![0; 512]);
        assert!(ImageDiskView::partition(reader.clone(), 0, 2, ViewOptions::default()).is_err());
        assert!(ImageDiskView::disk(reader.clone(), 1, ViewOptions::default()).is_err());
        let v = ImageDiskView::partition(reader, 0, 1, ViewOptions::default()).unwrap();
        assert_eq!(v.size(), 6 * MIB);
        assert_eq!(v.partition_offset(1), Some(MIB));
        let (h, e) = crate::gpt::parse_primary(&read(&v, 0, 34 * 512), 512).unwrap();
        assert_eq!(h.first_usable_lba, 2048);
        assert_eq!(
            (e[0].type_id, e[0].first_lba, e[0].last_lba, e[0].name.as_str()),
            (crate::gpt::BASIC_DATA, 2048, 10239, "L1")
        );
        check_partition_data(&v, MIB);
        // Tail padding up to the backup GPT (33 sectors) is empty.
        assert!(read(&v, 5 * MIB, MIB as usize - 33 * 512).iter().all(|&b| b == 0));
    }

    #[test]
    fn read_past_short_block_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let reader = image_with(dir.path(), PartitionStyle::Mbr { signature: 1 }, vec![0; 512], &[(0, 1000)], 512);
        let v = ImageDiskView::partition(reader, 0, 1, ViewOptions::default()).unwrap();
        assert!(read(&v, MIB + 2000, 100).iter().all(|&b| b == 0));
        let r = read(&v, MIB + 950, 100);
        assert_eq!(r[..50], pattern(1, 0)[950..1000]);
        assert!(r[50..].iter().all(|&b| b == 0));
    }

    #[test]
    fn bad_sector_size_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let layout = PartitionStyle::Gpt {
            disk_id: Uuid::nil(),
            first_usable_lba: 34,
            last_usable_lba: 0,
            max_partition_count: 128,
        };
        let reader = image_with(dir.path(), layout, vec![0; 512], &[(0, 1000)], 0);
        assert!(ImageDiskView::partition(reader.clone(), 0, 1, ViewOptions::default()).is_err());
        assert!(ImageDiskView::disk(reader, 0, ViewOptions::default()).is_err());
    }

    #[test]
    fn hostile_gpt_geometry_is_rejected_without_clamping() {
        for (count, first) in [(u32::MAX, 2048), (128, u64::MAX)] {
            let dir = tempfile::tempdir().unwrap();
            let layout = PartitionStyle::Gpt {
                disk_id: Uuid::nil(),
                first_usable_lba: first,
                last_usable_lba: 0,
                max_partition_count: count,
            };
            let reader = image(dir.path(), layout, vec![0; 512]);
            assert!(ImageDiskView::disk(reader, 0, ViewOptions::default()).is_err());
        }
    }

    #[test]
    fn disk_view_preserves_usable_boundary_with_compact_table() {
        let dir = tempfile::tempdir().unwrap();
        let layout = PartitionStyle::Gpt {
            disk_id: Uuid::nil(),
            first_usable_lba: 8192,
            last_usable_lba: 0,
            max_partition_count: 128,
        };
        let reader = image(dir.path(), layout, vec![0; 512]);
        let v = ImageDiskView::disk(reader, 0, ViewOptions::default()).unwrap();
        let (h, _) = crate::gpt::parse_primary(&read(&v, 0, 34 * 512), 512).unwrap();
        assert_eq!(h.first_usable_lba, 8192);
    }
}
