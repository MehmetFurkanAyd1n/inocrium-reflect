//! The JSON manifest describing what an image contains.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub image_id: Uuid,
    /// RFC 3339 timestamp of when the backup started.
    pub created: String,
    #[serde(default)]
    pub comment: String,
    pub tool_version: String,
    #[serde(default)]
    pub source: SourceInfo,
    pub block_size: u32,
    pub compression: CompressionInfo,
    #[serde(default)]
    pub encryption: Option<EncryptionInfo>,
    /// Backup definition that produced this image, used by retention.
    #[serde(default)]
    pub definition_id: Option<String>,
    pub disks: Vec<DiskManifest>,
}

impl Manifest {
    pub fn disk(&self, index: u32) -> Option<&DiskManifest> {
        self.disks.iter().find(|d| d.index == index)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub hostname: String,
    pub os_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionInfo {
    /// `"zstd"` or `"none"`.
    pub algorithm: String,
    pub level: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionInfo {
    pub cipher: String,
    pub kdf: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Only blocks the file system reports as allocated.
    Smart,
    /// Every sector.
    Forensic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum PartitionStyle {
    Mbr { signature: u32 },
    Gpt { disk_id: Uuid, first_usable_lba: u64, last_usable_lba: u64, max_partition_count: u32 },
    Raw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskManifest {
    /// Index of the disk within this image.
    pub index: u32,
    /// `\\.\PhysicalDriveN` number on the source machine.
    pub source_number: u32,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub serial: String,
    pub size: u64,
    pub sector_size: u32,
    #[serde(default)]
    pub physical_sector_size: u32,
    pub layout: PartitionStyle,
    pub partitions: Vec<PartitionManifest>,
    /// Unpartitioned regions captured in forensic mode.
    #[serde(default)]
    pub gaps: Vec<GapManifest>,
    /// Regions stored verbatim in the metadata (TRACK0, GPT backup area, EBRs).
    #[serde(default)]
    pub raw_regions: Vec<RawRegionInfo>,
}

impl DiskManifest {
    pub fn partition(&self, number: u32) -> Option<&PartitionManifest> {
        self.partitions.iter().find(|p| p.number == number)
    }
    pub fn captured_partitions(&self) -> impl Iterator<Item = &PartitionManifest> {
        self.partitions.iter().filter(|p| p.captured)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartitionManifest {
    /// Partition number as reported by Windows (1-based).
    pub number: u32,
    /// Stream holding the partition data (only meaningful when `captured`).
    pub stream: u32,
    /// Byte offset on the source disk.
    pub offset: u64,
    pub length: u64,
    /// Whether the partition contents are in the image. Partitions that were not selected are
    /// still listed so that the layout can be reproduced.
    pub captured: bool,
    pub capture: CaptureMode,
    #[serde(default)]
    pub gpt: Option<GptPartitionInfo>,
    #[serde(default)]
    pub mbr: Option<MbrPartitionInfo>,
    #[serde(default)]
    pub filesystem: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub drive_letter: Option<char>,
    /// Bytes in use according to the file system, when known.
    #[serde(default)]
    pub used_bytes: Option<u64>,
    /// Plain bytes stored in the image for this partition.
    #[serde(default)]
    pub stored_bytes: u64,
    #[serde(default)]
    pub is_system: bool,
    #[serde(default)]
    pub is_boot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GptPartitionInfo {
    pub type_id: Uuid,
    pub id: Uuid,
    pub name: String,
    pub attributes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MbrPartitionInfo {
    pub partition_type: u8,
    pub bootable: bool,
    /// Logical partition inside an extended partition.
    #[serde(default)]
    pub logical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapManifest {
    pub stream: u32,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRegionInfo {
    pub offset: u64,
    pub length: u64,
    #[serde(default)]
    pub kind: RawRegionKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawRegionKind {
    #[default]
    Track0,
    GptBackup,
    Ebr,
    Other,
}
