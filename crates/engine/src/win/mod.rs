//! Windows platform layer: VSS-backed backup sources, restore to physical disks and
//! partitions, VHDX export and loopback iSCSI mounts.

mod mount;
mod restore;
mod source;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blockdev::BlockDevice;
use irimg::ImageReader;
use mrimgx::MrimgxImage;

use crate::{Error, Result};

pub use mount::{MountInfo, MountManager, MountRequest, MountedVolume, IQN_PREFIX};
pub use restore::{
    export_vhdx, restore_image_disk, restore_image_partition, DiskRestoreOptions, DiskRestoreReport, PartitionTarget,
};
pub use source::{prepare_backup, DiskSelection, PrepareOptions, PreparedSources, Shadow};

impl From<winio::Error> for Error {
    fn from(e: winio::Error) -> Self {
        Error::Io(e.into())
    }
}

impl From<vss::Error> for Error {
    fn from(e: vss::Error) -> Self {
        Error::Io(io::Error::other(e))
    }
}

impl From<iscsi::Error> for Error {
    fn from(e: iscsi::Error) -> Self {
        Error::Io(io::Error::other(e))
    }
}

impl From<mrimgx::Error> for Error {
    fn from(e: mrimgx::Error) -> Self {
        match e {
            mrimgx::Error::Io(e) => Error::Io(e),
            e => Error::invalid(e.to_string()),
        }
    }
}

/// An image of either supported format.
#[derive(Clone)]
pub enum AnyImage {
    Irimg(Arc<ImageReader>),
    /// A Macrium Reflect X image (read-only: verify, mount, restore, export).
    Mrimgx(Arc<MrimgxImage>),
}

/// Geometry of one disk of an image.
#[derive(Debug, Clone, Copy)]
pub struct ImageDisk {
    pub size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub gpt: bool,
}

impl AnyImage {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        if MrimgxImage::is_mrimgx(path) {
            Ok(AnyImage::Mrimgx(Arc::new(MrimgxImage::open(path, password)?)))
        } else {
            Ok(AnyImage::Irimg(Arc::new(ImageReader::open(path, password)?)))
        }
    }

    pub fn disk_count(&self) -> usize {
        match self {
            AnyImage::Irimg(r) => r.manifest().disks.len(),
            AnyImage::Mrimgx(m) => m.info().disks.len(),
        }
    }

    pub fn disk(&self, index: u32) -> Option<ImageDisk> {
        match self {
            AnyImage::Irimg(r) => r.manifest().disk(index).map(|d| ImageDisk {
                size: d.size,
                sector_size: d.sector_size,
                physical_sector_size: d.physical_sector_size,
                gpt: matches!(d.layout, irimg::PartitionStyle::Gpt { .. }),
            }),
            AnyImage::Mrimgx(m) => m.info().disks.get(index as usize).map(|d| ImageDisk {
                size: d.size,
                sector_size: d.sector_size,
                physical_sector_size: d.sector_size,
                gpt: d.style.eq_ignore_ascii_case("gpt"),
            }),
        }
    }

    /// The files the image is stored in.
    pub fn files(&self, path: &Path) -> Vec<PathBuf> {
        match self {
            AnyImage::Irimg(r) => r.paths().to_vec(),
            AnyImage::Mrimgx(_) => vec![path.to_path_buf()],
        }
    }
}

/// Copies `len` bytes from `src` to `dst` at the same offsets, skipping all-zero chunks when
/// `skip_zeros` is set (the target already reads as zeros).
pub(crate) fn copy_device(
    src: &dyn BlockDevice,
    dst: &dyn BlockDevice,
    len: u64,
    skip_zeros: bool,
    rep: &mut crate::progress::Reporter,
    cancel: &crate::Cancel,
) -> Result<u64> {
    const CHUNK: u64 = 4 << 20;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut written = 0;
    let mut pos = 0;
    while pos < len {
        cancel.check()?;
        let n = CHUNK.min(len - pos) as usize;
        src.read_at(pos, &mut buf[..n])?;
        if !(skip_zeros && buf[..n].iter().all(|&b| b == 0)) {
            dst.write_at(pos, &buf[..n])?;
            written += n as u64;
        }
        pos += n as u64;
        rep.p.bytes_done = pos;
        rep.p.bytes_written = written;
        rep.tick();
    }
    dst.flush()?;
    rep.force();
    Ok(written)
}

/// Strips the `\\?\` prefix `canonicalize` adds; UNC paths are returned as `None`.
fn local_path(path: &Path) -> Option<String> {
    let p = path.canonicalize().ok()?;
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if s.starts_with(r"UNC\") || s.starts_with(r"\\") {
        return None;
    }
    Some(s.to_string())
}

/// The GUID paths of the volumes holding `paths`.
pub(crate) fn volumes_holding(paths: &[PathBuf]) -> Result<Vec<String>> {
    let volumes = winio::list_volumes()?;
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        let Some(p) = local_path(path) else { continue };
        let p = p.to_ascii_lowercase();
        // The volume whose mount point is the longest prefix of the path.
        let best = volumes
            .iter()
            .flat_map(|v| v.mount_points.iter().map(move |m| (m.to_ascii_lowercase(), v)))
            .filter(|(m, _)| p.starts_with(m.as_str()) || p == m.trim_end_matches('\\'))
            .max_by_key(|(m, _)| m.len());
        if let Some((_, v)) = best {
            if !out.iter().any(|g| g.eq_ignore_ascii_case(&v.guid_path)) {
                out.push(v.guid_path.clone());
            }
        }
    }
    Ok(out)
}

/// The physical disks the volumes holding `paths` live on.
pub fn disks_holding(paths: &[PathBuf]) -> Result<HashSet<u32>> {
    let mut out = HashSet::new();
    for guid in volumes_holding(paths)? {
        for (disk, _, _) in winio::volume_disk_extents(&guid)? {
            out.insert(disk);
        }
    }
    Ok(out)
}

/// Refuses to write to the disk Windows runs from.
fn refuse_system_disk(info: &winio::DiskInfo) -> Result<()> {
    if info.is_system {
        return Err(Error::invalid(format!(
            "disk {} holds the running Windows installation and cannot be a restore target",
            info.number
        )));
    }
    Ok(())
}
