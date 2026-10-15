//! Describing an opened image for listings and `inocrium info`.

use std::path::Path;

use chrono::{DateTime, Utc};
use ipc::proto::{BackupMode, DiskStyle, ImageDisk, ImagePartition, ImageSummary};
use irimg::{CaptureMode, ImageReader, PartitionStyle};
use uuid::Uuid;

use super::AnyImage;

pub fn file_time(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path).and_then(|m| m.modified()).map(DateTime::<Utc>::from).unwrap_or_default()
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

pub fn describe(path: &Path, img: &AnyImage) -> ImageSummary {
    match img {
        AnyImage::Irimg(r) => {
            let m = r.manifest();
            let header = ImageReader::peek_header(path).ok();
            let header_time = || header.as_ref().and_then(|h| DateTime::from_timestamp(h.created_unix, 0));
            ImageSummary {
                path: path.display().to_string(),
                created: DateTime::parse_from_rfc3339(&m.created)
                    .map(|t| t.with_timezone(&Utc))
                    .ok()
                    .or_else(header_time)
                    .unwrap_or_else(|| file_time(path)),
                comment: m.comment.clone(),
                size_on_disk: r.paths().iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum(),
                encrypted: m.encryption.is_some() || header.as_ref().is_some_and(|h| h.encrypted()),
                definition_id: m.definition_id.as_deref().and_then(|d| Uuid::parse_str(d).ok()),
                disks: m
                    .disks
                    .iter()
                    .map(|d| ImageDisk {
                        index: d.index,
                        size: d.size,
                        style: match d.layout {
                            PartitionStyle::Gpt { .. } => DiskStyle::Gpt,
                            PartitionStyle::Mbr { .. } => DiskStyle::Mbr,
                            PartitionStyle::Raw => DiskStyle::Raw,
                        },
                        partitions: d
                            .partitions
                            .iter()
                            .map(|p| ImagePartition {
                                number: p.number,
                                length: p.length,
                                file_system: non_empty(&p.filesystem),
                                label: non_empty(&p.label),
                                captured: p.captured,
                                mode: match p.capture {
                                    CaptureMode::Smart => BackupMode::Smart,
                                    CaptureMode::Forensic => BackupMode::Forensic,
                                },
                            })
                            .collect(),
                    })
                    .collect(),
                incomplete: crate::checkpoint_path(path).exists(),
            }
        }
        AnyImage::Mrimgx(x) => {
            let info = x.info();
            ImageSummary {
                path: path.display().to_string(),
                created: info
                    .created
                    .as_deref()
                    .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_else(|| file_time(path)),
                comment: info.comment.clone(),
                size_on_disk: std::fs::metadata(path).map_or(0, |m| m.len()),
                encrypted: info.encrypted,
                definition_id: None,
                disks: info
                    .disks
                    .iter()
                    .enumerate()
                    .map(|(i, d)| ImageDisk {
                        index: i as u32,
                        size: d.size,
                        style: match d.style.as_str() {
                            "gpt" => DiskStyle::Gpt,
                            "mbr" => DiskStyle::Mbr,
                            _ => DiskStyle::Raw,
                        },
                        partitions: d
                            .partitions
                            .iter()
                            .map(|p| ImagePartition {
                                number: p.number,
                                length: p.length,
                                file_system: non_empty(&p.filesystem),
                                label: non_empty(&p.label),
                                captured: true,
                                mode: BackupMode::Smart,
                            })
                            .collect(),
                    })
                    .collect(),
                incomplete: false,
            }
        }
    }
}
