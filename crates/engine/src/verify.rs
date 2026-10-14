//! Image verification: every stored block is decoded and its hash / tag checked.

use irimg::{ImageReader, VerifyReport};

use crate::progress::{Cancel, ProgressFn, Reporter};
use crate::{Error, Result};

/// Sum of the plain sizes of all stored blocks.
pub fn stored_bytes(reader: &ImageReader) -> u64 {
    let m = reader.manifest();
    m.disks
        .iter()
        .flat_map(|d| {
            d.captured_partitions().map(|p| p.stream).chain(d.gaps.iter().map(|g| g.stream)).map(move |s| (d.index, s))
        })
        .flat_map(|(disk, stream)| reader.stream_entries(disk, stream))
        .map(|e| e.plain_len as u64)
        .sum()
}

pub fn verify(reader: &ImageReader, progress: ProgressFn, cancel: &Cancel) -> Result<VerifyReport> {
    let mut rep = Reporter::new(progress, "verify", stored_bytes(reader));
    let result = reader.verify(|p| {
        rep.p.bytes_done = p.bytes_done;
        rep.p.detail = format!("{} / {} blocks", p.blocks_done, p.blocks_total);
        rep.tick();
        !cancel.is_cancelled()
    });
    match result {
        Ok(report) => {
            rep.force();
            Ok(report)
        }
        Err(_) if cancel.is_cancelled() => Err(Error::Cancelled),
        Err(e) => Err(e.into()),
    }
}
