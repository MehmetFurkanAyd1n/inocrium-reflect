use anyhow::Result;
use ipc::proto;
use serde::Serialize;

pub fn value(value: &impl Serialize, json: bool) -> Result<()> {
    let value = serde_json::to_value(value)?;
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else if let Some(s) = value.as_str() {
        println!("{s}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

pub fn disks(disks: &[proto::Disk], json: bool) -> Result<()> {
    if json {
        return value(&disks, true);
    }
    for disk in disks {
        println!("Disk {}  {}  {} bytes  {:?}", disk.number, disk.model, disk.size, disk.style);
        for p in &disk.partitions {
            println!(
                "  Partition {}  {}  {}  {}  {} bytes  used {}",
                p.number,
                p.letter.as_deref().unwrap_or("-"),
                p.file_system.as_deref().unwrap_or("-"),
                p.label.as_deref().unwrap_or("-"),
                p.length,
                p.used_bytes.map_or_else(|| "unknown".into(), |v| v.to_string())
            );
        }
    }
    Ok(())
}

pub fn image(image: &proto::ImageSummary, json: bool) -> Result<()> {
    if json {
        return value(image, true);
    }
    println!("Image {}", image.path);
    println!(
        "  created {}  {} bytes on disk{}",
        image.created.to_rfc3339(),
        image.size_on_disk,
        if image.encrypted { "  encrypted" } else { "" }
    );
    if !image.comment.is_empty() {
        println!("  comment {}", image.comment);
    }
    if image.incomplete {
        println!("  incomplete: the backup did not finish");
    }
    for disk in &image.disks {
        println!("Disk {}  {:?}  {} bytes", disk.index, disk.style, disk.size);
        for p in &disk.partitions {
            println!(
                "  Partition {}  {}  {}  {} bytes{}",
                p.number,
                p.file_system.as_deref().unwrap_or("-"),
                p.label.as_deref().unwrap_or("-"),
                p.length,
                if p.captured { "" } else { "  (not captured)" }
            );
        }
    }
    Ok(())
}

pub fn mount(mount: &proto::Mount, json: bool) -> Result<()> {
    if json {
        return value(mount, true);
    }
    println!("Mount {}: {}", mount.id, mount.letters.join(", "));
    Ok(())
}
