//! Image naming and retention of full images in a destination folder.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone};
use irimg::ImageReader;
use serde::{Deserialize, Serialize};

use crate::backup::checkpoint_path;
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Retention {
    /// Keep the newest N images.
    KeepLast(u32),
    /// Keep images younger than N days.
    KeepDays(u32),
}

/// `{prefix}-{YYYYMMDD-HHMMSS}.irimg`.
pub fn image_name<Tz: TimeZone>(prefix: &str, created: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!("{prefix}-{}.{}", created.format("%Y%m%d-%H%M%S"), irimg::EXTENSION)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFile {
    pub path: PathBuf,
    pub created_unix: i64,
    /// An interrupted backup that may still be resumed.
    pub incomplete: bool,
}

/// Images in `dir` named `{prefix}-*.irimg`, oldest first. Files whose header cannot be read
/// are ignored.
pub fn find_images(dir: &Path, prefix: &str) -> Result<Vec<ImageFile>> {
    let mut out = Vec::new();
    let start = format!("{prefix}-");
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let is_image =
            path.extension().is_some_and(|e| e.eq_ignore_ascii_case(irimg::EXTENSION)) && name.starts_with(&start);
        if !is_image {
            continue;
        }
        let Ok(header) = ImageReader::peek_header(&path) else { continue };
        let incomplete = checkpoint_path(&path).exists();
        out.push(ImageFile { path, created_unix: header.created_unix, incomplete });
    }
    out.sort_by(|a, b| a.created_unix.cmp(&b.created_unix).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// The images `policy` would delete. `keep` (the image just written) and incomplete images
/// are never deleted.
pub fn expired(images: &[ImageFile], policy: Retention, now_unix: i64, keep: Option<&Path>) -> Vec<PathBuf> {
    let candidates: Vec<&ImageFile> =
        images.iter().filter(|i| !i.incomplete && keep.is_none_or(|k| !same_path(&i.path, k))).collect();
    match policy {
        Retention::KeepLast(n) => {
            // `keep` counts towards N.
            let n = (n as usize).saturating_sub(keep.is_some() as usize);
            let excess = candidates.len().saturating_sub(n);
            candidates[..excess].iter().map(|i| i.path.clone()).collect()
        }
        Retention::KeepDays(days) => {
            let cutoff = now_unix - days as i64 * 86_400;
            candidates.iter().filter(|i| i.created_unix < cutoff).map(|i| i.path.clone()).collect()
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Applies `policy` to the images of `prefix` in `dir`, deleting every file of an expired
/// image (all split parts). Returns the deleted images.
pub fn apply_retention(
    dir: &Path,
    prefix: &str,
    policy: Retention,
    now_unix: i64,
    keep: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let images = find_images(dir, prefix)?;
    let mut deleted = Vec::new();
    for path in expired(&images, policy, now_unix, keep) {
        let mut ok = true;
        for f in irimg::image_files(&path) {
            if let Err(e) = std::fs::remove_file(&f) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("could not delete {}: {e}", f.display());
                    ok = false;
                }
            }
        }
        if ok {
            deleted.push(path);
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(name: &str, t: i64, incomplete: bool) -> ImageFile {
        ImageFile { path: PathBuf::from(name), created_unix: t, incomplete }
    }

    #[test]
    fn policies() {
        let list = [img("a", 100, false), img("b", 200, true), img("c", 300, false), img("d", 400, false)];
        assert_eq!(expired(&list, Retention::KeepLast(2), 0, Some(Path::new("d"))), [PathBuf::from("a")]);
        assert_eq!(expired(&list, Retention::KeepLast(1), 0, None), [PathBuf::from("a"), PathBuf::from("c")]);
        assert_eq!(expired(&list, Retention::KeepLast(0), 0, Some(Path::new("d"))).len(), 2);
        let now = 400 + 86_400;
        assert_eq!(
            expired(&list, Retention::KeepDays(1), now, Some(Path::new("d"))),
            [PathBuf::from("a"), PathBuf::from("c")]
        );
        let t = chrono::Utc.with_ymd_and_hms(2026, 10, 14, 9, 5, 7).unwrap();
        assert_eq!(image_name("sys", &t), "sys-20261014-090507.irimg");
    }
}
