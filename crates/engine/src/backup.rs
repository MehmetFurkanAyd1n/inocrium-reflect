//! Disk and partition imaging.
//!
//! One reader thread walks the selected blocks of every stream in order, a pool of encoder
//! threads compresses / encrypts them and the calling thread appends them to the image (in
//! whatever order they finish) while reporting progress and periodically writing a
//! checkpoint (`name.irimg.ckpt`). An interrupted backup is continued with [`backup`] and the
//! checkpoint; blocks already in the image are skipped.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blockdev::BlockDevice;
use irimg::{
    BlockRef, CaptureMode, Checkpoint, CompressionInfo, DiskManifest, EncryptionInfo, FinishSummary, ImageWriter,
    Manifest, RawRegion, SourceInfo, WriterOptions,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::extents::{zero_outside, Extents};
use crate::progress::{Cancel, ProgressFn, Reporter};
use crate::{fs, plan, Error, Result};

const MIB: u64 = 1024 * 1024;

/// A disk to image.
pub struct SourceDisk {
    /// The whole disk; raw regions and forensic gaps are read from here.
    pub device: Arc<dyn BlockDevice>,
    /// Layout of the disk. `captured`, `stream`, `capture`, `gaps` and `raw_regions` are
    /// filled in by the engine from `partitions` and `forensic_gaps`.
    pub manifest: DiskManifest,
    /// Partitions to capture.
    pub partitions: Vec<SourcePartition>,
    /// Also capture unpartitioned space (forensic whole-disk images).
    pub forensic_gaps: bool,
}

pub struct SourcePartition {
    pub number: u32,
    /// The partition contents (usually a VSS snapshot of its volume).
    pub device: Arc<dyn BlockDevice>,
    /// Allocated byte ranges for smart capture; `None` captures every sector.
    pub used: Option<Extents>,
}

#[derive(Debug, Clone)]
pub struct BackupOptions {
    pub writer: WriterOptions,
    pub comment: String,
    pub definition_id: Option<String>,
    pub source: SourceInfo,
    /// Encoder threads; 0 picks one per core (at most 8).
    pub threads: usize,
    pub checkpoint_interval: Duration,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            writer: WriterOptions::default(),
            comment: String::new(),
            definition_id: None,
            source: SourceInfo::default(),
            threads: 0,
            checkpoint_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
pub struct BackupOutcome {
    pub image: PathBuf,
    pub manifest: Manifest,
    pub summary: FinishSummary,
}

/// What the engine keeps in a checkpoint besides the caller's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EngineState {
    created: String,
    disks: Vec<DiskManifest>,
}

/// `name.irimg` → `name.irimg.ckpt`.
pub fn checkpoint_path(image: &Path) -> PathBuf {
    let mut s = image.as_os_str().to_owned();
    s.push(".ckpt");
    PathBuf::from(s)
}

/// The checkpoint of an interrupted backup of `image`, if there is one.
pub fn load_checkpoint(image: &Path) -> Result<Option<Checkpoint>> {
    match std::fs::read(checkpoint_path(image)) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The caller state stored with [`backup`]'s `caller_state`.
pub fn checkpoint_caller_state(ckpt: &Checkpoint) -> &serde_json::Value {
    &ckpt.user["caller"]
}

fn save_checkpoint(image: &Path, ckpt: &Checkpoint) -> Result<()> {
    let path = checkpoint_path(image);
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let mut file = std::fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(ckpt)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &path)?;
    // Flush the renamed file too, so the replacement survives a power loss.
    std::fs::OpenOptions::new().write(true).open(&path)?.sync_all()?;
    Ok(())
}

/// Number of encoder threads for a requested count (0 = automatic).
pub(crate) fn thread_count(requested: usize) -> usize {
    if requested > 0 {
        requested
    } else {
        std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 8)
    }
}

/// One stream to read: a captured partition or a forensic gap.
struct Item {
    disk: u32,
    stream: u32,
    device: Arc<dyn BlockDevice>,
    base: u64,
    len: u64,
    used: Option<Extents>,
}

impl Item {
    fn blocks(&self, bs: u64) -> u64 {
        self.len.div_ceil(bs)
    }

    fn range(&self, block: u64, bs: u64) -> Range<u64> {
        let start = block * bs;
        start..(start + bs).min(self.len)
    }

    fn selected(&self, r: &Range<u64>) -> bool {
        self.used.as_ref().is_none_or(|u| u.intersects(r.clone()))
    }
}

/// Fills in the engine-owned parts of a source disk's manifest.
fn plan_disk(index: u32, src: &SourceDisk, used: &HashMap<u32, Extents>) -> Result<DiskManifest> {
    let mut m = src.manifest.clone();
    m.index = index;
    for p in &mut m.partitions {
        let s = src.partitions.iter().find(|s| s.number == p.number);
        p.captured = s.is_some();
        p.stream = p.number;
        p.capture = if s.is_some_and(|s| s.used.is_some()) { CaptureMode::Smart } else { CaptureMode::Forensic };
        p.used_bytes = used.get(&p.number).map(Extents::total);
        p.stored_bytes = 0;
    }
    for s in &src.partitions {
        if m.partition(s.number).is_none() {
            return Err(Error::invalid(format!("partition {} is not in the layout of disk {}", s.number, index)));
        }
    }
    m.raw_regions = plan::raw_regions(src.device.as_ref(), &m)?;
    m.gaps = if src.forensic_gaps { plan::gaps(&m, &m.raw_regions) } else { vec![] };
    Ok(m)
}

/// Smart extents widened by what must always be kept: the first and last MiB of the
/// partition (boot sectors and their backups) and, for NTFS, everything past the bitmap.
fn smart_extents(device: &dyn BlockDevice, used: &Extents, len: u64) -> Result<Extents> {
    let mut e = used.clip(len);
    e.add(0..MIB.min(len));
    e.add(len.saturating_sub(MIB)..len);
    if len >= 512 {
        let mut boot = vec![0u8; 512];
        device.read_at(0, &mut boot)?;
        if let Some(vol) = fs::ntfs_volume_bytes(&boot) {
            e.add(vol.min(len)..len);
        }
    }
    Ok(e)
}

fn same_plan(a: &DiskManifest, b: &DiskManifest) -> bool {
    let parts = |d: &DiskManifest| -> Vec<(u32, u64, u64, bool, CaptureMode)> {
        d.partitions.iter().map(|p| (p.number, p.offset, p.length, p.captured, p.capture)).collect()
    };
    let gaps =
        |d: &DiskManifest| -> Vec<(u32, u64, u64)> { d.gaps.iter().map(|g| (g.stream, g.offset, g.length)).collect() };
    a.size == b.size
        && a.sector_size == b.sector_size
        && a.source_number == b.source_number
        && a.serial == b.serial
        && a.layout == b.layout
        && parts(a) == parts(b)
        && a.partitions
            .iter()
            .zip(&b.partitions)
            .all(|(a, b)| a.stream == b.stream && a.gpt == b.gpt && a.mbr == b.mbr && a.filesystem == b.filesystem)
        && gaps(a) == gaps(b)
}

/// Images `disks` into `dest` (`name.irimg`). With `resume`, continues the interrupted backup
/// the checkpoint describes; the sources must be the same (e.g. the same persistent snapshot).
/// `caller_state` is stored in every checkpoint (see [`checkpoint_caller_state`]).
///
/// On cancellation a checkpoint is left next to the image and [`Error::Cancelled`] returned.
pub fn backup(
    dest: &Path,
    disks: &[SourceDisk],
    opts: &BackupOptions,
    resume: Option<&Checkpoint>,
    caller_state: serde_json::Value,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<BackupOutcome> {
    if disks.is_empty() {
        return Err(Error::invalid("nothing to back up"));
    }

    // Plan.
    let mut items = Vec::new();
    let mut manifests = Vec::new();
    for (i, src) in disks.iter().enumerate() {
        let mut used = HashMap::new();
        for s in &src.partitions {
            let p =
                src.manifest.partition(s.number).ok_or_else(|| Error::invalid(format!("no partition {}", s.number)))?;
            if s.device.size() < p.length {
                return Err(Error::invalid(format!("partition {} source is smaller than the partition", s.number)));
            }
            if let Some(u) = &s.used {
                used.insert(s.number, smart_extents(s.device.as_ref(), u, p.length)?);
            }
        }
        let m = plan_disk(i as u32, src, &used)?;
        for s in &src.partitions {
            let p = m.partition(s.number).unwrap();
            items.push(Item {
                disk: i as u32,
                stream: p.stream,
                device: s.device.clone(),
                base: 0,
                len: p.length,
                used: used.remove(&s.number),
            });
        }
        for g in &m.gaps {
            items.push(Item {
                disk: i as u32,
                stream: g.stream,
                device: src.device.clone(),
                base: g.offset,
                len: g.length,
                used: None,
            });
        }
        manifests.push(m);
    }

    let (mut writer, created) = match resume {
        Some(ckpt) => {
            let state: EngineState = serde_json::from_value(ckpt.user["engine"].clone())
                .map_err(|_| Error::invalid("checkpoint was not written by this engine"))?;
            if state.disks.len() != manifests.len() || !state.disks.iter().zip(&manifests).all(|(a, b)| same_plan(a, b))
            {
                return Err(Error::invalid("the source disks changed since the interrupted backup"));
            }
            (ImageWriter::resume(dest, ckpt, opts.writer.password.as_deref())?, state.created)
        }
        None => (ImageWriter::create(dest, opts.writer.clone())?, chrono::Local::now().to_rfc3339()),
    };
    let bs = writer.block_size() as u64;
    let engine_state = serde_json::to_value(EngineState { created: created.clone(), disks: manifests.clone() })?;
    let user = serde_json::json!({ "engine": engine_state, "caller": caller_state });
    if resume.is_some() {
        // Record the resumed run's nonce counter before anything is sealed. A later resume then
        // starts past every nonce this run can use, even if it stops before its first periodic
        // checkpoint, instead of relying on the random offset alone.
        save_checkpoint(dest, &writer.checkpoint(user.clone())?)?;
    }

    // Totals and blocks already in the image.
    let mut total = 0u64;
    let mut already = 0u64;
    let mut stored: HashMap<(u32, u32), u64> = HashMap::new();
    let mut done_blocks: HashSet<(u32, u32, u64)> = HashSet::new();
    for it in &items {
        for b in 0..it.blocks(bs) {
            let r = it.range(b, bs);
            if !it.selected(&r) {
                continue;
            }
            let n = r.end - r.start;
            total += n;
            if resume.is_some() && writer.contains(it.disk, it.stream, b) {
                already += n;
                *stored.entry((it.disk, it.stream)).or_default() += n;
                done_blocks.insert((it.disk, it.stream, b));
            }
        }
    }

    let mut rep = Reporter::new(progress, "backup", total);
    rep.p.bytes_done = already;
    rep.force();

    let threads = thread_count(opts.threads);
    let read_done = AtomicU64::new(already);
    let stop = AtomicBool::new(false);
    let reader_finished = AtomicBool::new(false);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let (raw_tx, raw_rx) = crossbeam_channel::bounded::<(BlockRef, Vec<u8>)>(threads * 2);
    let (enc_tx, enc_rx) = crossbeam_channel::bounded(threads * 2);
    let encoder = writer.encoder();
    let mut append_error = None;
    let mut last_ckpt = Instant::now();

    std::thread::scope(|scope| {
        let (items, done_blocks, read_done, stop, failure, reader_finished) =
            (&items, &done_blocks, &read_done, &stop, &failure, &reader_finished);
        scope.spawn(move || {
            let mut buf = vec![0u8; bs as usize];
            for it in items {
                for b in 0..it.blocks(bs) {
                    if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                        return;
                    }
                    let r = it.range(b, bs);
                    if !it.selected(&r) {
                        continue;
                    }
                    let n = (r.end - r.start) as usize;
                    if done_blocks.contains(&(it.disk, it.stream, b)) {
                        continue;
                    }
                    let data = &mut buf[..n];
                    if let Err(e) = it.device.read_at(it.base + r.start, data) {
                        *failure.lock() = Some(Error::Io(e));
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                    let skip = match &it.used {
                        Some(u) => {
                            zero_outside(data, &u.within(r.clone()));
                            false
                        }
                        // Forensic: zero blocks are not stored; restores zero-fill them.
                        None => data.iter().all(|&x| x == 0),
                    };
                    if !skip {
                        let at = BlockRef { disk: it.disk, stream: it.stream, block: b };
                        if raw_tx.send((at, data.to_vec())).is_err() {
                            return;
                        }
                    }
                    read_done.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            reader_finished.store(true, Ordering::SeqCst);
        });
        for _ in 0..threads {
            let (rx, tx, enc) = (raw_rx.clone(), enc_tx.clone(), encoder.clone());
            scope.spawn(move || {
                for (at, data) in rx {
                    if tx.send(enc.encode(at, &data)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(raw_rx);
        drop(enc_tx);

        for block in enc_rx {
            if append_error.is_some() {
                continue; // drain so the workers can exit
            }
            let at = block.at;
            let (plain, written) = (block.plain_len as u64, block.data.len() as u64);
            if let Err(e) = writer.append(block) {
                append_error = Some(e);
                stop.store(true, Ordering::Relaxed);
                continue;
            }
            *stored.entry((at.disk, at.stream)).or_default() += plain;
            rep.p.bytes_written += written;
            rep.p.bytes_done = read_done.load(Ordering::Relaxed);
            rep.tick();
            if last_ckpt.elapsed() >= opts.checkpoint_interval {
                match writer.checkpoint(user.clone()).map_err(Error::from).and_then(|c| save_checkpoint(dest, &c)) {
                    Ok(()) => last_ckpt = Instant::now(),
                    Err(e) => tracing::warn!("could not write checkpoint: {e}"),
                }
            }
        }
    });

    if let Some(e) = append_error {
        return Err(e.into());
    }
    if let Some(e) = failure.into_inner() {
        return Err(e);
    }
    if !reader_finished.load(Ordering::SeqCst) {
        let ckpt = writer.checkpoint(user)?;
        save_checkpoint(dest, &ckpt)?;
        return Err(Error::Cancelled);
    }

    // Metadata.
    let mut raw = Vec::new();
    for (i, (src, m)) in disks.iter().zip(&mut manifests).enumerate() {
        for r in &m.raw_regions {
            let mut data = vec![0u8; r.length as usize];
            src.device.read_at(r.offset, &mut data)?;
            raw.push(RawRegion { disk: i as u32, offset: r.offset, data });
        }
        for p in &mut m.partitions {
            p.stored_bytes = if p.captured { stored.get(&(i as u32, p.stream)).copied().unwrap_or(0) } else { 0 };
        }
    }
    let manifest = Manifest {
        format_version: irimg::FORMAT_VERSION,
        image_id: writer.image_id(),
        created,
        comment: opts.comment.clone(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        source: opts.source.clone(),
        block_size: bs as u32,
        compression: CompressionInfo { algorithm: "zstd".into(), level: opts.writer.compression_level },
        encryption: opts
            .writer
            .password
            .as_ref()
            .map(|_| EncryptionInfo { cipher: "aes-256-gcm".into(), kdf: "argon2id".into() }),
        definition_id: opts.definition_id.clone(),
        disks: manifests,
    };
    let summary = writer.finish(&manifest, &raw)?;
    match std::fs::remove_file(checkpoint_path(dest)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not delete checkpoint: {e}"),
    }
    rep.p.bytes_done = total;
    rep.p.bytes_written = summary.total_bytes;
    rep.force();
    Ok(BackupOutcome { image: dest.to_path_buf(), manifest, summary })
}
