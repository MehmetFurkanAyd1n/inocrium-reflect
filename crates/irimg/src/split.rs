//! Naming of split image files: `name.irimg`, `name.i01`, `name.i02`, ...

use std::path::{Path, PathBuf};

/// Strips the extension of any file belonging to an image (`x.irimg`, `x.i07`, `x.ckpt`...).
pub fn image_base_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let lower = ext.to_ascii_lowercase();
        let is_part = lower.len() >= 3 && lower.starts_with('i') && lower[1..].chars().all(|c| c.is_ascii_digit());
        if lower == crate::EXTENSION || is_part {
            p.set_extension("");
        }
    }
    p
}

/// Path of file `file_no` of the image whose base path (without extension) is `base`.
pub fn image_file_path(base: &Path, file_no: u32) -> PathBuf {
    let mut p = base.as_os_str().to_owned();
    if file_no == 0 {
        p.push(".");
        p.push(crate::EXTENSION);
    } else {
        p.push(format!(".i{file_no:02}"));
    }
    PathBuf::from(p)
}

/// Lists the existing files of an image in order, stopping at the first gap.
pub fn image_files(path: &Path) -> Vec<PathBuf> {
    let base = image_base_path(path);
    let mut out = Vec::new();
    for n in 0.. {
        let p = image_file_path(&base, n);
        if !p.is_file() {
            break;
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming() {
        let base = image_base_path(Path::new(r"D:\b\disk.irimg"));
        assert_eq!(base, PathBuf::from(r"D:\b\disk"));
        assert_eq!(image_base_path(Path::new(r"D:\b\disk.i12")), base);
        assert_eq!(image_file_path(&base, 0), PathBuf::from(r"D:\b\disk.irimg"));
        assert_eq!(image_file_path(&base, 1), PathBuf::from(r"D:\b\disk.i01"));
        assert_eq!(image_file_path(&base, 123), PathBuf::from(r"D:\b\disk.i123"));
        assert_eq!(image_base_path(Path::new("a.b.irimg")), PathBuf::from("a.b"));
    }
}
