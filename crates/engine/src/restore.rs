//! Writing an image back to a disk or partition.
//!
//! These functions only write bytes; taking the target offline, dismounting its volumes and
//! telling Windows about the new layout (and growing the file system after `extend_last`) is
//! done by the platform layer.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use blockdev::{disk_table, BlockDevice};
use irimg::{CaptureMode, DiskManifest, ImageReader, PartitionStyle, RawRegionKind};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::backup::thread_count;
use crate::progress::{Cancel, ProgressFn, Reporter};
use crate::{Error, Result};

const MIB: u64 = 1024 * 1024;

/// Whether blocks that are not stored in the image are written as zeros.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZeroFill {
    /// For forensic streams, where unstored blocks were all zero on the source. Unstored
    /// blocks of smart streams are unallocated and left alone.
    #[default]
    Auto,
    Always,
    /// The target is known to read as zeros (e.g. a new virtual disk).
    Never,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Give the restored disk a new MBR signature / GPT GUIDs.
    pub rewrite_identity: bool,
    pub zero_fill: ZeroFill,
    /// Grow the last partition to the end of a larger target disk.
    pub extend_last: bool,
    /// Decoder threads; 0 picks one per core (at most 8).
    pub threads: usize,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// The layout written to the target (partition offsets and lengths as on the target).
    pub layout: DiskManifest,
    pub bytes_written: u64,
    /// The partition that was grown by `extend_last`.
    pub extended: Option<u32>,
}

/// One stream to write.
struct Job {
    stream: u32,
    target_offset: u64,
    len: u64,
    zero_fill: bool,
}

fn zero_fill_for(policy: ZeroFill, capture: CaptureMode) -> bool {
    match policy {
        ZeroFill::Auto => capture == CaptureMode::Forensic,
        ZeroFill::Always => true,
        ZeroFill::Never => false,
    }
}

/// Writes the streams of `jobs` in parallel. Returns the bytes written.
fn run_jobs(
    reader: &ImageReader,
    disk: u32,
    jobs: &[Job],
    target: &dyn BlockDevice,
    threads: usize,
    rep: &mut Reporter,
    cancel: &Cancel,
) -> Result<u64> {
    let bs = reader.block_size() as u64;
    // Block `g` of the whole restore lives in the job whose prefix range contains it.
    let mut prefix = Vec::with_capacity(jobs.len() + 1);
    prefix.push(0u64);
    for j in jobs {
        prefix.push(prefix.last().unwrap() + j.len.div_ceil(bs));
    }
    let total_blocks = *prefix.last().unwrap();

    let next = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let written = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let running = AtomicUsize::new(threads);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let zeros = vec![0u8; bs as usize];

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let work = || -> Result<()> {
                    loop {
                        if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                            return Ok(());
                        }
                        let g = next.fetch_add(1, Ordering::Relaxed);
                        if g >= total_blocks {
                            return Ok(());
                        }
                        let ji = prefix.partition_point(|&p| p <= g) - 1;
                        let job = &jobs[ji];
                        let b = g - prefix[ji];
                        let start = b * bs;
                        let n = (bs.min(job.len - start)) as usize;
                        match reader.read_block(disk, job.stream, b)? {
                            Some(data) => {
                                let mut buf = data;
                                buf.resize(n, 0);
                                target.write_at(job.target_offset + start, &buf)?;
                                written.fetch_add(n as u64, Ordering::Relaxed);
                            }
                            None if job.zero_fill => {
                                target.write_at(job.target_offset + start, &zeros[..n])?;
                                written.fetch_add(n as u64, Ordering::Relaxed);
                            }
                            None => {}
                        }
                        done.fetch_add(n as u64, Ordering::Relaxed);
                    }
                };
                if let Err(e) = work() {
                    failure.lock().get_or_insert(e);
                    stop.store(true, Ordering::Relaxed);
                }
                running.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while running.load(Ordering::SeqCst) > 0 {
            std::thread::sleep(Duration::from_millis(100));
            rep.p.bytes_done = done.load(Ordering::Relaxed);
            rep.p.bytes_written = written.load(Ordering::Relaxed);
            rep.tick();
        }
    });

    if let Some(e) = failure.into_inner() {
        return Err(e);
    }
    cancel.check()?;
    rep.p.bytes_done = done.load(Ordering::Relaxed);
    rep.p.bytes_written = written.load(Ordering::Relaxed);
    rep.force();
    Ok(written.into_inner())
}

/// The layout of `d` on a disk of `target_size` bytes, with the last captured partition
/// grown to the end of the disk (1 MiB aligned) when `extend_last` is set.
pub fn target_layout(d: &DiskManifest, target_size: u64, extend_last: bool) -> (DiskManifest, Option<u32>) {
    let mut layout = d.clone();
    if !extend_last || target_size <= d.size {
        return (layout, None);
    }
    let ss = d.sector_size as u64;
    let usable_end = match &d.layout {
        // The backup GPT goes right after the last usable sector.
        PartitionStyle::Gpt { .. } => disk_table(d, None, target_size, false).get(1).map_or(0, |t| t.0),
        PartitionStyle::Mbr { .. } => target_size,
        PartitionStyle::Raw => return (layout, None),
    };
    let Some(p) = layout.partitions.iter_mut().filter(|p| p.captured).max_by_key(|p| p.offset + p.length) else {
        return (layout, None);
    };
    let mut end = usable_end / MIB * MIB;
    if matches!(d.layout, PartitionStyle::Mbr { .. }) {
        if p.mbr.is_some_and(|m| m.logical) {
            return (layout, None); // would need the extended container and EBR chain moved too
        }
        end = end.min((p.offset / ss + u32::MAX as u64) * ss);
    }
    if end <= p.offset + p.length {
        return (layout, None);
    }
    p.length = end - p.offset;
    let n = p.number;
    (layout, Some(n))
}

/// Restores image disk `disk` onto `target`, which must be at least as large and use the
/// same logical sector size. Only captured partitions are listed in the new partition table.
pub fn restore_disk(
    reader: &ImageReader,
    disk: u32,
    target: &dyn BlockDevice,
    opts: &RestoreOptions,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<RestoreReport> {
    let d = reader.manifest().disk(disk).ok_or_else(|| Error::invalid(format!("the image has no disk {disk}")))?;
    let size = target.size();
    if size < d.size {
        return Err(Error::invalid(format!(
            "the target disk ({size} bytes) is smaller than the imaged disk ({} bytes)",
            d.size
        )));
    }
    if target.block_size() != d.sector_size {
        return Err(Error::invalid(format!(
            "the target disk has {}-byte sectors, the image {}-byte sectors",
            target.block_size(),
            d.sector_size
        )));
    }
    if target.read_only() {
        return Err(Error::invalid("the target is read-only"));
    }

    let mut jobs: Vec<Job> = d
        .captured_partitions()
        .map(|p| Job {
            stream: p.stream,
            target_offset: p.offset,
            len: p.length,
            zero_fill: zero_fill_for(opts.zero_fill, p.capture),
        })
        .collect();
    jobs.extend(d.gaps.iter().map(|g| Job {
        stream: g.stream,
        target_offset: g.offset,
        len: g.length,
        zero_fill: zero_fill_for(opts.zero_fill, CaptureMode::Forensic),
    }));
    let raw_total: u64 = reader.raw_regions(disk).map(|r| r.data.len() as u64).sum();
    let mut rep = Reporter::new(progress, "restore", jobs.iter().map(|j| j.len).sum::<u64>() + raw_total);

    // Raw regions first: TRACK0 also carries the old table, which is replaced last.
    let mut track0 = None;
    let mut raw_written = 0;
    for r in reader.raw_regions(disk) {
        let kind = d.raw_regions.iter().find(|i| i.offset == r.offset).map_or(RawRegionKind::Other, |i| i.kind);
        if kind == RawRegionKind::Track0 && r.offset == 0 {
            track0 = Some(r.data.as_slice());
        }
        // The backup GPT only belongs at the old position on a disk of the same size.
        if kind == RawRegionKind::GptBackup && size != d.size {
            continue;
        }
        if r.offset + r.data.len() as u64 <= size {
            target.write_at(r.offset, &r.data)?;
            raw_written += r.data.len() as u64;
        }
    }

    let threads = thread_count(opts.threads);
    let data_written = run_jobs(reader, disk, &jobs, target, threads, &mut rep, cancel)?;

    let (layout, extended) = target_layout(d, size, opts.extend_last);
    let mut table_written = 0;
    for (offset, data) in disk_table(&layout, track0, size, opts.rewrite_identity) {
        target.write_at(offset, &data)?;
        table_written += data.len() as u64;
    }
    target.flush()?;
    rep.p.bytes_done = rep.p.bytes_total;
    rep.force();
    Ok(RestoreReport { layout, bytes_written: raw_written + data_written + table_written, extended })
}

/// Restores partition `number` of image disk `disk` onto `target` (the target partition or
/// free space), which must be at least as large as the partition. Returns the bytes written.
pub fn restore_partition(
    reader: &ImageReader,
    disk: u32,
    number: u32,
    target: &dyn BlockDevice,
    opts: &RestoreOptions,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<u64> {
    let d = reader.manifest().disk(disk).ok_or_else(|| Error::invalid(format!("the image has no disk {disk}")))?;
    let p = d
        .partition(number)
        .filter(|p| p.captured)
        .ok_or_else(|| Error::invalid(format!("partition {number} is not in the image")))?;
    if target.size() < p.length {
        return Err(Error::invalid(format!(
            "the target ({} bytes) is smaller than the partition ({} bytes)",
            target.size(),
            p.length
        )));
    }
    if target.read_only() {
        return Err(Error::invalid("the target is read-only"));
    }
    let jobs = [Job {
        stream: p.stream,
        target_offset: 0,
        len: p.length,
        zero_fill: zero_fill_for(opts.zero_fill, p.capture),
    }];
    let mut rep = Reporter::new(progress, "restore", p.length);
    let n = run_jobs(reader, disk, &jobs, target, thread_count(opts.threads), &mut rep, cancel)?;
    target.flush()?;
    Ok(n)
}
