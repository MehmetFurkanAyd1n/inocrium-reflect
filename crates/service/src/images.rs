//! Finding, describing, opening and deleting images.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engine::win::{describe, file_time, AnyImage};
use ipc::proto::{ImageSummary, RpcError, BAD_PASSWORD, BUSY, INVALID_PARAMS, NOT_FOUND, PASSWORD_REQUIRED};
use irimg::ImageReader;
use mrimgx::MrimgxImage;
use parking_lot::Mutex;

use crate::store::{internal, Store};

const MRIMGX_EXTENSION: &str = "mrimgx";

/// Connects to the SMB share holding `path` with the stored credential, if it is on one.
/// Without a stored credential the connection uses the service account's identity.
pub fn connect_share(store: &Store, path: &Path) -> Result<(), RpcError> {
    let Some(root) = winio::share_root(path) else { return Ok(()) };
    let cred = store.credential_for(&root);
    let (user, password) = match &cred {
        Some((u, p)) => (Some(u.as_str()), Some(p.as_str())),
        None => (None, None),
    };
    winio::connect_share(&root, user, password).map_err(|e| internal(format!("cannot connect to {root}: {e}")))
}

fn is_irimg(path: &Path) -> bool {
    has_extension(path, irimg::EXTENSION)
}

fn is_mrimgx(path: &Path) -> bool {
    has_extension(path, MRIMGX_EXTENSION)
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Comparable identity of an image path (Windows paths are case-insensitive).
pub fn path_key(path: &Path) -> String {
    let base = if is_irimg(path) { irimg::image_base_path(path) } else { path.to_path_buf() };
    base.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Images that running jobs read or write, so they cannot be deleted underneath them.
#[derive(Default)]
pub struct InUse(Mutex<HashMap<String, usize>>);

pub struct InUseGuard {
    owner: Arc<InUse>,
    key: String,
}

impl InUse {
    pub fn acquire(self: &Arc<Self>, path: &Path) -> InUseGuard {
        let key = path_key(path);
        *self.0.lock().entry(key.clone()).or_default() += 1;
        InUseGuard { owner: self.clone(), key }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.0.lock().contains_key(&path_key(path))
    }
}

impl Drop for InUseGuard {
    fn drop(&mut self) {
        let mut map = self.owner.0.lock();
        if let Some(n) = map.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

fn irimg_error(e: irimg::Error) -> RpcError {
    match e {
        irimg::Error::PasswordRequired => RpcError::new(PASSWORD_REQUIRED, "the image is encrypted"),
        irimg::Error::WrongPassword => RpcError::new(BAD_PASSWORD, "wrong password"),
        irimg::Error::MissingFile(f) => RpcError::new(NOT_FOUND, format!("missing image file {f}")),
        irimg::Error::BadMagic => RpcError::new(INVALID_PARAMS, "not an inocrium image"),
        e => internal(e),
    }
}

fn mrimgx_error(e: mrimgx::Error) -> RpcError {
    match e {
        mrimgx::Error::PasswordRequired => RpcError::new(PASSWORD_REQUIRED, "the image is encrypted"),
        mrimgx::Error::WrongPassword => RpcError::new(BAD_PASSWORD, "wrong password"),
        mrimgx::Error::MissingFile(f) => RpcError::new(NOT_FOUND, format!("missing image file {f}")),
        e => internal(e),
    }
}

fn require_image(path: &Path) -> Result<(), RpcError> {
    if !is_irimg(path) && !is_mrimgx(path) {
        return Err(RpcError::invalid_params(format!("{} is not an .irimg or .mrimgx image", path.display())));
    }
    if !path.is_file() {
        return Err(RpcError::new(NOT_FOUND, format!("{} not found", path.display())));
    }
    Ok(())
}

fn open_with(path: &Path, password: Option<&str>) -> Result<AnyImage, RpcError> {
    if is_mrimgx(path) {
        Ok(AnyImage::Mrimgx(Arc::new(MrimgxImage::open(path, password).map_err(mrimgx_error)?)))
    } else {
        Ok(AnyImage::Irimg(Arc::new(ImageReader::open(path, password).map_err(irimg_error)?)))
    }
}

/// Opens an image for a job. Without a password, the passwords of definitions writing to the
/// image's folder are tried, so scheduled images can be verified and mounted without retyping.
pub fn open(store: &Store, path: &Path, password: Option<&str>) -> Result<AnyImage, RpcError> {
    let _ = connect_share(store, path);
    require_image(path)?;
    match open_with(path, password) {
        Err(e) if e.code == PASSWORD_REQUIRED && password.is_none() => {
            for pw in definition_passwords(store, path) {
                if let Ok(img) = open_with(path, Some(&pw)) {
                    return Ok(img);
                }
            }
            Err(e)
        }
        other => other,
    }
}

/// The password that opens an image, found the same way as [`open`]; for jobs that open the
/// image themselves (mounts). `None` means the image is not encrypted.
pub fn resolve_password(store: &Store, path: &Path, password: Option<&str>) -> Result<Option<String>, RpcError> {
    let _ = connect_share(store, path);
    require_image(path)?;
    match open_with(path, password) {
        Ok(_) => Ok(password.map(String::from)),
        Err(e) if e.code == PASSWORD_REQUIRED && password.is_none() => definition_passwords(store, path)
            .into_iter()
            .find(|pw| open_with(path, Some(pw)).is_ok())
            .map(Some)
            .ok_or(e),
        Err(e) => Err(e),
    }
}

fn definition_passwords(store: &Store, image: &Path) -> Vec<String> {
    let Some(dir) = image.parent().map(path_key) else { return Vec::new() };
    store
        .all_stored()
        .into_iter()
        .filter(|s| s.def.encrypt && path_key(Path::new(s.def.destination.trim())) == dir)
        .filter_map(|s| store.definition_password(s.def.id))
        .collect()
}

/// Folders `images.list` searches when the client names none: the configured image folders
/// and every definition's destination.
pub fn default_folders(store: &Store) -> Vec<String> {
    let mut out = store.image_folders();
    out.extend(store.definitions().into_iter().map(|d| d.destination));
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| !f.trim().is_empty() && seen.insert(path_key(Path::new(f.trim()))));
    out
}

/// Images directly inside each folder, newest first. Unreadable files are skipped; folders
/// the client named explicitly must exist.
pub fn list(store: &Store, folders: Option<Vec<String>>) -> Result<Vec<ImageSummary>, RpcError> {
    let explicit = folders.is_some();
    let folders = folders.unwrap_or_else(|| default_folders(store));
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for folder in folders {
        let dir = PathBuf::from(folder.trim());
        if let Err(e) = connect_share(store, &dir) {
            tracing::warn!("{}", e.message);
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if explicit => return Err(RpcError::new(NOT_FOUND, format!("{}: {e}", dir.display()))),
            Err(e) => {
                tracing::debug!("skipping image folder {}: {e}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !(is_irimg(&path) || is_mrimgx(&path)) || !path.is_file() || !seen.insert(path_key(&path)) {
                continue;
            }
            match summarize(&path, None) {
                Ok(s) => out.push(s),
                Err(e) => tracing::debug!("skipping {}: {}", path.display(), e.message),
            }
        }
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// Authenticated metadata of one image.
pub fn inspect(store: &Store, path: &Path, password: Option<&str>) -> Result<ImageSummary, RpcError> {
    let img = open(store, path, password)?;
    Ok(describe(path, &img))
}

/// What can be told about an image without a password. Encrypted images keep their metadata
/// sealed, so their summary has no disks.
fn summarize(path: &Path, password: Option<&str>) -> Result<ImageSummary, RpcError> {
    if is_mrimgx(path) {
        if MrimgxImage::needs_password(path).map_err(mrimgx_error)? && password.is_none() {
            return Ok(sealed(path, file_time(path), true, false));
        }
    } else {
        let header = ImageReader::peek_header(path).map_err(irimg_error)?;
        let created = DateTime::from_timestamp(header.created_unix, 0).unwrap_or_else(|| file_time(path));
        let incomplete = engine::checkpoint_path(path).exists();
        if header.encrypted() && password.is_none() {
            return Ok(sealed(path, created, true, incomplete));
        }
        match ImageReader::open(path, password) {
            Ok(r) => return Ok(describe(path, &AnyImage::Irimg(Arc::new(r)))),
            // Still being written, or interrupted before the footer.
            Err(irimg::Error::CorruptFooter) => return Ok(sealed(path, created, header.encrypted(), true)),
            Err(e) => return Err(irimg_error(e)),
        }
    }
    Ok(describe(path, &open_with(path, password)?))
}

fn sealed(path: &Path, created: DateTime<Utc>, encrypted: bool, incomplete: bool) -> ImageSummary {
    ImageSummary {
        path: path.display().to_string(),
        created,
        comment: String::new(),
        size_on_disk: size_on_disk(path),
        encrypted,
        definition_id: None,
        disks: Vec::new(),
        incomplete,
    }
}

fn size_on_disk(path: &Path) -> u64 {
    let files = if is_irimg(path) { irimg::image_files(path) } else { vec![path.to_path_buf()] };
    files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum()
}

/// Deletes every file of an image (split parts and resume checkpoint included). Images that
/// are mounted or used by a running job are refused.
pub fn delete(store: &Store, path: &Path, in_use: &InUse, mounted: &[PathBuf]) -> Result<(), RpcError> {
    let _ = connect_share(store, path);
    require_image(path)?;
    let key = path_key(path);
    if mounted.iter().any(|m| path_key(m) == key) {
        return Err(RpcError::new(BUSY, "the image is mounted"));
    }
    if in_use.contains(path) {
        return Err(RpcError::new(BUSY, "the image is in use by a running job"));
    }
    let mut files = if is_irimg(path) { irimg::image_files(path) } else { vec![path.to_path_buf()] };
    if is_irimg(path) {
        files.push(engine::checkpoint_path(path));
    }
    for f in files {
        match std::fs::remove_file(&f) {
            Ok(()) => tracing::info!("deleted {}", f.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(internal(format!("cannot delete {}: {e}", f.display()))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use uuid::Uuid;

    fn write_image(dir: &Path, name: &str, password: Option<&str>, comment: &str) -> PathBuf {
        let path = dir.join(name);
        let opts = irimg::WriterOptions {
            password: password.map(String::from),
            kdf: irimg::KdfParams::insecure_fast(),
            ..Default::default()
        };
        let w = irimg::ImageWriter::create(&path, opts).unwrap();
        let manifest = irimg::Manifest {
            format_version: irimg::FORMAT_VERSION,
            image_id: w.image_id(),
            created: "2026-01-02T03:04:05Z".into(),
            comment: comment.into(),
            tool_version: "test".into(),
            source: Default::default(),
            block_size: w.block_size(),
            compression: irimg::CompressionInfo { algorithm: "zstd".into(), level: 3 },
            encryption: None,
            definition_id: Some(Uuid::from_u128(7).to_string()),
            disks: vec![irimg::DiskManifest {
                index: 0,
                source_number: 1,
                model: "Test".into(),
                serial: String::new(),
                size: 1 << 20,
                sector_size: 512,
                physical_sector_size: 4096,
                layout: irimg::PartitionStyle::Raw,
                partitions: vec![],
                gaps: vec![],
                raw_regions: vec![],
            }],
        };
        w.finish(&manifest, &[]).unwrap();
        path
    }

    fn store(dir: &Path) -> Store {
        let paths = Paths::new(dir.join("data"));
        paths.create(false).unwrap();
        Store::load(&paths)
    }

    #[test]
    fn list_and_inspect() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let imgs = dir.path().join("imgs");
        std::fs::create_dir(&imgs).unwrap();
        let plain = write_image(&imgs, "a-1.irimg", None, "hello");
        let locked = write_image(&imgs, "b-1.irimg", Some("pw"), "secret");
        std::fs::write(imgs.join("junk.irimg"), b"not an image").unwrap();
        std::fs::write(imgs.join("notes.txt"), b"x").unwrap();

        let all = list(&store, Some(vec![imgs.display().to_string()])).unwrap();
        assert_eq!(all.len(), 2);
        let a = all.iter().find(|s| s.path.ends_with("a-1.irimg")).unwrap();
        assert_eq!((a.comment.as_str(), a.encrypted, a.incomplete, a.disks.len()), ("hello", false, false, 1));
        assert_eq!(a.definition_id, Some(Uuid::from_u128(7)));
        assert_eq!(a.created.to_rfc3339(), "2026-01-02T03:04:05+00:00");
        assert!(a.size_on_disk > 0);
        let b = all.iter().find(|s| s.path.ends_with("b-1.irimg")).unwrap();
        assert!(b.encrypted && b.disks.is_empty() && b.comment.is_empty());

        assert!(list(&store, Some(vec![])).unwrap().is_empty());
        assert_eq!(
            list(&store, Some(vec![dir.path().join("nope").display().to_string()])).unwrap_err().code,
            NOT_FOUND
        );
        assert!(list(&store, None).unwrap().is_empty());

        assert_eq!(inspect(&store, &plain, None).unwrap().comment, "hello");
        assert_eq!(inspect(&store, &locked, None).unwrap_err().code, PASSWORD_REQUIRED);
        assert_eq!(inspect(&store, &locked, Some("nope")).unwrap_err().code, BAD_PASSWORD);
        assert_eq!(inspect(&store, &locked, Some("pw")).unwrap().comment, "secret");
        assert_eq!(inspect(&store, &imgs.join("missing.irimg"), None).unwrap_err().code, NOT_FOUND);
    }

    #[test]
    fn in_use_counts_and_keys() {
        let u = Arc::new(InUse::default());
        let a = u.acquire(Path::new(r"D:\Img\X.irimg"));
        let b = u.acquire(Path::new(r"d:/img/x.IRIMG"));
        assert!(u.contains(Path::new(r"D:\IMG\x.i01")) || u.contains(Path::new(r"d:\img\x.irimg")));
        drop(a);
        assert!(u.contains(Path::new(r"d:\img\x.irimg")));
        drop(b);
        assert!(!u.contains(Path::new(r"d:\img\x.irimg")));
    }

    #[test]
    fn delete_refuses_mounted_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, b"x").unwrap();
        assert_eq!(delete(&store, &other, &InUse::default(), &[]).unwrap_err().code, INVALID_PARAMS);
        let img = dir.path().join("a-1.irimg");
        std::fs::write(&img, b"x").unwrap();
        std::fs::write(engine::checkpoint_path(&img), b"x").unwrap();
        assert_eq!(delete(&store, &img, &InUse::default(), std::slice::from_ref(&img)).unwrap_err().code, BUSY);
        let used = Arc::new(InUse::default());
        let guard = used.acquire(&img);
        assert_eq!(delete(&store, &img, &used, &[]).unwrap_err().code, BUSY);
        drop(guard);
        delete(&store, &img, &used, &[]).unwrap();
        assert!(!img.exists() && !engine::checkpoint_path(&img).exists());
        assert_eq!(delete(&store, &img, &used, &[]).unwrap_err().code, NOT_FOUND);
    }
}
