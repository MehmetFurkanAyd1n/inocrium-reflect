//! Making a virtual machine start Windows Boot Manager from an image whose EFI system partition
//! defaults to another boot loader. A VM has none of the source computer's UEFI boot entries,
//! so its firmware runs the fallback loader `\EFI\BOOT\BOOTX64.EFI`, which on dual-boot disks is
//! often GRUB. The patch points that directory entry at `\EFI\Microsoft\Boot\bootmgfw.efi`
//! (sharing its clusters), as byte patches over the exposed disk.

use std::io;

use blockdev::BlockDevice;

use crate::ntfs::{volume_offsets, Patch};

const ENTRY: usize = 32;
const ATTR_LFN: u8 = 0x0F;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_ARCHIVE: u8 = 0x20;
/// Larger file allocation tables are not plausible for an EFI system partition.
const MAX_FAT_BYTES: u64 = 64 * 1024 * 1024;

const WINDOWS_LOADER: [&str; 4] = ["EFI", "Microsoft", "Boot", "bootmgfw.efi"];
const FALLBACK_DIR: [&str; 2] = ["EFI", "BOOT"];
const FALLBACK_NAME: &str = "BOOTX64.EFI";

#[derive(Debug)]
pub enum WindowsBoot {
    /// No FAT volume on the disk holds Windows Boot Manager.
    NotFound,
    /// The fallback loader already is Windows Boot Manager.
    AlreadyDefault,
    /// Patches that make it so.
    Patches(Vec<Patch>),
}

/// Finds the EFI system partition holding Windows Boot Manager on `disk` and, unless the
/// fallback loader already starts it, returns the patches that make it the fallback loader.
pub fn windows_boot(disk: &dyn BlockDevice) -> io::Result<WindowsBoot> {
    for base in volume_offsets(disk)? {
        let Some(fat) = Fat::open(disk, base)? else { continue };
        let Some(loader) = fat.find(fat.root(), &WINDOWS_LOADER)? else { continue };
        if loader.attr & ATTR_DIRECTORY != 0 {
            continue;
        }
        let dir = fat
            .find(fat.root(), &FALLBACK_DIR)?
            .filter(|d| d.attr & ATTR_DIRECTORY != 0)
            .ok_or_else(|| invalid("the EFI system partition has no \\EFI\\BOOT directory"))?;
        let dir = Dir::Clusters(dir.cluster);
        let entries = fat.entries(&dir)?;
        if let Some(fallback) = entries.iter().find(|e| e.name.eq_ignore_ascii_case(FALLBACK_NAME)) {
            if fallback.cluster == loader.cluster || fat.same_contents(fallback, &loader)? {
                return Ok(WindowsBoot::AlreadyDefault);
            }
            let mut raw = fallback.raw;
            set_target(&mut raw, &loader);
            return Ok(WindowsBoot::Patches(vec![(fallback.at, raw.to_vec())]));
        }
        // No fallback loader yet: add a short entry in the first free slot.
        let free = fat
            .slots(&dir)?
            .into_iter()
            .find(|(_, raw)| raw[0] == 0x00 || raw[0] == 0xE5)
            .ok_or_else(|| invalid("\\EFI\\BOOT has no free directory entry"))?;
        let mut raw = [0u8; ENTRY];
        raw[..11].copy_from_slice(b"BOOTX64 EFI");
        raw[11] = ATTR_ARCHIVE;
        set_target(&mut raw, &loader);
        return Ok(WindowsBoot::Patches(vec![(free.0, raw.to_vec())]));
    }
    Ok(WindowsBoot::NotFound)
}

fn set_target(raw: &mut [u8; ENTRY], target: &Entry) {
    raw[0x14..0x16].copy_from_slice(&((target.cluster >> 16) as u16).to_le_bytes());
    raw[0x1A..0x1C].copy_from_slice(&(target.cluster as u16).to_le_bytes());
    raw[0x1C..0x20].copy_from_slice(&target.size.to_le_bytes());
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    attr: u8,
    cluster: u32,
    size: u32,
    /// Device offset of the short entry.
    at: u64,
    raw: [u8; ENTRY],
}

#[derive(Clone, Copy)]
enum Dir {
    /// The FAT12/16 root directory region.
    Fixed {
        offset: u64,
        len: u64,
    },
    Clusters(u32),
}

struct Fat<'a> {
    disk: &'a dyn BlockDevice,
    cluster: u64,
    /// Device offset of cluster 2.
    data: u64,
    fat32: bool,
    root: Dir,
    table: Vec<u8>,
    clusters: u32,
}

impl<'a> Fat<'a> {
    /// The FAT16 or FAT32 file system at `base`, or `None` for anything else. Like UEFI
    /// firmware, FAT32 is recognized by a zero 16-bit FAT size.
    fn open(disk: &'a dyn BlockDevice, base: u64) -> io::Result<Option<Self>> {
        if base + 512 > disk.size() {
            return Ok(None);
        }
        let mut b = [0u8; 512];
        disk.read_at(base, &mut b)?;
        let u16_at = |at: usize| u64::from(u16::from_le_bytes([b[at], b[at + 1]]));
        let u32_at = |at: usize| u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()));
        let bps = u16_at(0x0B);
        let spc = u64::from(b[0x0D]);
        let (reserved, fats, root_entries) = (u16_at(0x0E), u64::from(b[0x10]), u16_at(0x11));
        if b[510..512] != [0x55, 0xAA]
            || !matches!(bps, 512 | 1024 | 2048 | 4096)
            || !spc.is_power_of_two()
            || reserved == 0
            || fats == 0
        {
            return Ok(None);
        }
        let fat32 = u16_at(0x16) == 0;
        let fat_sectors = if fat32 { u32_at(0x24) } else { u16_at(0x16) };
        let total = if u16_at(0x13) != 0 { u16_at(0x13) } else { u32_at(0x20) };
        let root_sectors = (root_entries * ENTRY as u64).div_ceil(bps);
        let data_sector = reserved + fats * fat_sectors + root_sectors;
        if fat_sectors == 0 || total <= data_sector || fat_sectors * bps > MAX_FAT_BYTES {
            return Ok(None);
        }
        let clusters = ((total - data_sector) / spc).min(0x0FFF_FFF0) as u32;
        if !fat32 && clusters < 4085 {
            return Ok(None); // FAT12
        }
        if base + total * bps > disk.size() {
            return Err(invalid("the FAT volume extends past the end of the disk"));
        }
        let mut table = vec![0u8; (fat_sectors * bps) as usize];
        disk.read_at(base + reserved * bps, &mut table)?;
        let root = if fat32 {
            Dir::Clusters(u32_at(0x2C) as u32)
        } else {
            Dir::Fixed { offset: base + (reserved + fats * fat_sectors) * bps, len: root_entries * ENTRY as u64 }
        };
        Ok(Some(Self { disk, cluster: bps * spc, data: base + data_sector * bps, fat32, root, table, clusters }))
    }

    fn root(&self) -> Dir {
        self.root
    }

    /// The clusters of a chain starting at `first`.
    fn chain(&self, first: u32) -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            if c < 2 || c - 2 >= self.clusters || out.len() > self.clusters as usize {
                return Err(invalid(format!("invalid cluster chain at {c}")));
            }
            out.push(c);
            let next = if self.fat32 {
                let at = 4 * c as usize;
                u32::from_le_bytes(
                    self.table.get(at..at + 4).ok_or_else(|| invalid("FAT too short"))?.try_into().unwrap(),
                ) & 0x0FFF_FFFF
            } else {
                let at = 2 * c as usize;
                let v = u16::from_le_bytes(
                    self.table.get(at..at + 2).ok_or_else(|| invalid("FAT too short"))?.try_into().unwrap(),
                );
                if v >= 0xFFF8 {
                    0x0FFF_FFFF
                } else {
                    u32::from(v)
                }
            };
            if next >= 0x0FFF_FFF8 {
                return Ok(out);
            }
            c = next;
        }
    }

    /// Every 32-byte slot of a directory with its device offset.
    fn slots(&self, dir: &Dir) -> io::Result<Vec<(u64, [u8; ENTRY])>> {
        let extents: Vec<(u64, u64)> = match *dir {
            Dir::Fixed { offset, len } => vec![(offset, len)],
            Dir::Clusters(first) => self
                .chain(first)?
                .into_iter()
                .map(|c| (self.data + u64::from(c - 2) * self.cluster, self.cluster))
                .collect(),
        };
        let mut out = Vec::new();
        for (offset, len) in extents {
            let mut buf = vec![0u8; len as usize];
            self.disk.read_at(offset, &mut buf)?;
            for (i, raw) in buf.as_chunks::<ENTRY>().0.iter().enumerate() {
                out.push((offset + (i * ENTRY) as u64, *raw));
            }
        }
        Ok(out)
    }

    /// The files and directories of a directory, by long name where there is one.
    fn entries(&self, dir: &Dir) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        // Long name parts: sequence number, checksum of the short name, characters.
        let mut long: Vec<(u8, u8, [u16; 13])> = Vec::new();
        for (at, raw) in self.slots(dir)? {
            match raw[0] {
                0x00 => break,
                0xE5 => {
                    long.clear();
                    continue;
                }
                _ => {}
            }
            let attr = raw[11];
            if attr == ATTR_LFN {
                let mut chars = [0u16; 13];
                for (i, at) in (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2)).enumerate() {
                    chars[i] = u16::from_le_bytes([raw[at], raw[at + 1]]);
                }
                if raw[0] & 0x40 != 0 {
                    long.clear();
                }
                long.push((raw[0] & 0x1F, raw[13], chars));
                continue;
            }
            if attr & ATTR_VOLUME != 0 {
                long.clear();
                continue;
            }
            // A long name belongs to this entry only if it carries the short name's checksum.
            let checksum = raw[..11].iter().fold(0u8, |s, b| s.rotate_right(1).wrapping_add(*b));
            let name = if !long.is_empty() && long.iter().all(|(_, sum, _)| *sum == checksum) {
                long_name(&mut long)
            } else {
                short_name(&raw)
            };
            long.clear();
            let cluster = u32::from(u16::from_le_bytes([raw[0x1A], raw[0x1B]]))
                | if self.fat32 { u32::from(u16::from_le_bytes([raw[0x14], raw[0x15]])) << 16 } else { 0 };
            out.push(Entry {
                name,
                attr,
                cluster,
                size: u32::from_le_bytes(raw[0x1C..0x20].try_into().unwrap()),
                at,
                raw,
            });
        }
        Ok(out)
    }

    /// Resolves `path` (case-insensitive) below `dir`.
    fn find(&self, dir: Dir, path: &[&str]) -> io::Result<Option<Entry>> {
        let mut dir = dir;
        for (i, part) in path.iter().enumerate() {
            let Some(e) = self.entries(&dir)?.into_iter().find(|e| e.name.to_lowercase() == part.to_lowercase()) else {
                return Ok(None);
            };
            if i + 1 == path.len() {
                return Ok(Some(e));
            }
            if e.attr & ATTR_DIRECTORY == 0 || e.cluster == 0 {
                return Ok(None);
            }
            dir = Dir::Clusters(e.cluster);
        }
        Ok(None)
    }

    fn read_file(&self, e: &Entry) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(e.size as usize);
        if e.size > 0 {
            for c in self.chain(e.cluster)? {
                let mut buf = vec![0u8; self.cluster as usize];
                self.disk.read_at(self.data + u64::from(c - 2) * self.cluster, &mut buf)?;
                out.extend_from_slice(&buf);
                if out.len() >= e.size as usize {
                    break;
                }
            }
        }
        if out.len() < e.size as usize {
            return Err(invalid(format!("{} is shorter than its size", e.name)));
        }
        out.truncate(e.size as usize);
        Ok(out)
    }

    fn same_contents(&self, a: &Entry, b: &Entry) -> io::Result<bool> {
        Ok(a.size == b.size && self.read_file(a)? == self.read_file(b)?)
    }
}

/// Joins long name parts (stored last part first) and cuts at the terminator.
fn long_name(parts: &mut [(u8, u8, [u16; 13])]) -> String {
    parts.sort_by_key(|(seq, _, _)| *seq);
    let units: Vec<u16> =
        parts.iter().flat_map(|(_, _, c)| c.iter().copied()).take_while(|&u| u != 0 && u != 0xFFFF).collect();
    String::from_utf16_lossy(&units)
}

fn short_name(raw: &[u8; ENTRY]) -> String {
    let base = String::from_utf8_lossy(&raw[..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&raw[8..11]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockdev::MemDevice;

    const RESERVED: u64 = 32;
    const FAT_SECTORS: u64 = 64;
    const DATA: u64 = (RESERVED + 2 * FAT_SECTORS) * 512;

    fn short(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; ENTRY] {
        let mut e = [0u8; ENTRY];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[0x14..0x16].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[0x1A..0x1C].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// Long name entries (last part first) followed by the short entry.
    fn named(long: &str, short_entry: [u8; ENTRY]) -> Vec<[u8; ENTRY]> {
        let sum = short_entry[..11].iter().fold(0u8, |s, b| s.rotate_right(1).wrapping_add(*b));
        let mut units: Vec<u16> = long.encode_utf16().collect();
        units.push(0);
        while !units.len().is_multiple_of(13) {
            units.push(0xFFFF);
        }
        let parts = units.len() / 13;
        let mut out = Vec::new();
        for seq in (1..=parts).rev() {
            let mut e = [0u8; ENTRY];
            e[0] = seq as u8 | if seq == parts { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            let chars = &units[(seq - 1) * 13..seq * 13];
            for (i, at) in (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2)).enumerate() {
                e[at..at + 2].copy_from_slice(&chars[i].to_le_bytes());
            }
            out.push(e);
        }
        out.push(short_entry);
        out
    }

    struct Image {
        dev: MemDevice,
    }

    impl Image {
        fn cluster(c: u32) -> u64 {
            DATA + u64::from(c - 2) * 512
        }
        fn link(&self, c: u32, next: u32) {
            for copy in 0..2 {
                let at = (RESERVED + copy * FAT_SECTORS) * 512 + 4 * u64::from(c);
                self.dev.write_at(at, &next.to_le_bytes()).unwrap();
            }
        }
        fn dir(&self, c: u32, entries: &[[u8; ENTRY]]) {
            self.link(c, 0x0FFF_FFFF);
            let bytes: Vec<u8> = entries.iter().flatten().copied().collect();
            self.dev.write_at(Self::cluster(c), &bytes).unwrap();
        }
        fn file(&self, first: u32, data: &[u8]) {
            let clusters = data.len().div_ceil(512) as u32;
            for i in 0..clusters {
                self.link(first + i, if i + 1 == clusters { 0x0FFF_FFFF } else { first + i + 1 });
            }
            self.dev.write_at(Self::cluster(first), data).unwrap();
        }
        fn apply(&self, patches: &[Patch]) {
            for (at, data) in patches {
                self.dev.write_at(*at, data).unwrap();
            }
        }
    }

    const BOOTMGFW: &[u8] = &[0x4D; 700];

    /// A FAT32 EFI system partition (without partition table) with GRUB as the fallback loader.
    fn esp(fallback: Option<&[u8]>) -> Image {
        let total = RESERVED + 2 * FAT_SECTORS + 4096;
        let img = Image { dev: MemDevice::new(total * 512, 512) };
        let mut b = [0u8; 512];
        b[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        b[3..11].copy_from_slice(b"MSDOS5.0");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 1;
        b[0x0E..0x10].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        b[0x10] = 2;
        b[0x20..0x24].copy_from_slice(&(total as u32).to_le_bytes());
        b[0x24..0x28].copy_from_slice(&(FAT_SECTORS as u32).to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&2u32.to_le_bytes());
        b[82..90].copy_from_slice(b"FAT32   ");
        b[510] = 0x55;
        b[511] = 0xAA;
        img.dev.write_at(0, &b).unwrap();
        img.dir(2, &[short(b"EFI        ", ATTR_DIRECTORY, 3, 0)]);
        let mut efi = vec![short(b".          ", ATTR_DIRECTORY, 3, 0), short(b"..         ", ATTR_DIRECTORY, 0, 0)];
        efi.push(short(b"BOOT       ", ATTR_DIRECTORY, 4, 0));
        efi.extend(named("Microsoft", short(b"MICROS~1   ", ATTR_DIRECTORY, 5, 0)));
        img.dir(3, &efi);
        let mut boot = vec![short(b".          ", ATTR_DIRECTORY, 4, 0), short(b"..         ", ATTR_DIRECTORY, 3, 0)];
        if let Some(data) = fallback {
            boot.push(short(b"BOOTX64 EFI", ATTR_ARCHIVE, 7, data.len() as u32));
            img.file(7, data);
        }
        img.dir(4, &boot);
        img.dir(5, &[short(b"BOOT       ", ATTR_DIRECTORY, 6, 0)]);
        img.dir(6, &named("bootmgfw.efi", short(b"BOOTMGFWEFI", ATTR_ARCHIVE, 0x10, BOOTMGFW.len() as u32)));
        img.file(0x10, BOOTMGFW);
        img
    }

    fn fallback_contents(img: &Image) -> Vec<u8> {
        let fat = Fat::open(&img.dev, 0).unwrap().unwrap();
        let e = fat.find(fat.root(), &["efi", "boot", "bootx64.efi"]).unwrap().unwrap();
        fat.read_file(&e).unwrap()
    }

    #[test]
    fn points_the_fallback_loader_at_windows_boot_manager() {
        let img = esp(Some(b"grub"));
        assert_eq!(fallback_contents(&img), b"grub");
        let WindowsBoot::Patches(patches) = windows_boot(&img.dev).unwrap() else { panic!("expected patches") };
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].1.len(), ENTRY);
        img.apply(&patches);
        assert_eq!(fallback_contents(&img), BOOTMGFW);
        assert!(matches!(windows_boot(&img.dev).unwrap(), WindowsBoot::AlreadyDefault));
    }

    #[test]
    fn adds_a_missing_fallback_loader() {
        let img = esp(None);
        let WindowsBoot::Patches(patches) = windows_boot(&img.dev).unwrap() else { panic!("expected patches") };
        img.apply(&patches);
        assert_eq!(fallback_contents(&img), BOOTMGFW);
    }

    #[test]
    fn leaves_windows_only_and_non_windows_disks() {
        let img = esp(Some(BOOTMGFW));
        assert!(matches!(windows_boot(&img.dev).unwrap(), WindowsBoot::AlreadyDefault));

        let img = esp(Some(b"grub"));
        // Delete bootmgfw.efi: both its long name parts and its short entry.
        for slot in 0..2 {
            img.dev.write_at(Image::cluster(6) + slot * ENTRY as u64, &[0xE5]).unwrap();
        }
        assert!(matches!(windows_boot(&img.dev).unwrap(), WindowsBoot::NotFound));
        assert!(matches!(windows_boot(&MemDevice::new(1024 * 1024, 512)).unwrap(), WindowsBoot::NotFound));
    }

    #[test]
    fn long_names_need_a_matching_checksum() {
        let img = esp(Some(b"grub"));
        let fat = Fat::open(&img.dev, 0).unwrap().unwrap();
        let names: Vec<String> = fat.entries(&Dir::Clusters(3)).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, [".", "..", "BOOT", "Microsoft"]);
        // Corrupt the checksum of the "Microsoft" long name: the short name is used instead.
        img.dev.write_at(Image::cluster(3) + 3 * ENTRY as u64 + 13, &[0]).unwrap();
        let names: Vec<String> = fat.entries(&Dir::Clusters(3)).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names[3], "MICROS~1");
        assert!(matches!(windows_boot(&img.dev).unwrap(), WindowsBoot::NotFound));
    }
}

/// Runs against a real disk prepared by an elevated test script: `INOCRIUM_TEST_ESP_DISK` is its
/// number, and `INOCRIUM_TEST_ESP_EXPECT` is `patches` (another fallback loader, as on a
/// dual-boot disk) or `already` (an image disk exposed with Windows Boot Manager as fallback).
#[cfg(all(test, windows))]
mod real_disk {
    use super::*;

    struct Overlay<'a> {
        inner: &'a dyn BlockDevice,
        patches: Vec<Patch>,
    }

    impl BlockDevice for Overlay<'_> {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn block_size(&self) -> u32 {
            self.inner.block_size()
        }
        fn read_only(&self) -> bool {
            true
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.inner.read_at(offset, buf)?;
            for (at, data) in &self.patches {
                for (i, b) in data.iter().enumerate() {
                    if let Some(rel) = (at + i as u64).checked_sub(offset).filter(|r| *r < buf.len() as u64) {
                        buf[rel as usize] = *b;
                    }
                }
            }
            Ok(())
        }
        fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }

    /// Contents of the fallback loader and of Windows Boot Manager.
    fn loaders(disk: &dyn BlockDevice) -> (Vec<u8>, Vec<u8>) {
        for base in volume_offsets(disk).unwrap() {
            let Some(fat) = Fat::open(disk, base).unwrap() else { continue };
            let Some(windows) = fat.find(fat.root(), &WINDOWS_LOADER).unwrap() else { continue };
            let fallback = fat.find(fat.root(), &["EFI", "BOOT", FALLBACK_NAME]).unwrap().expect("a fallback loader");
            return (fat.read_file(&fallback).unwrap(), fat.read_file(&windows).unwrap());
        }
        panic!("no EFI system partition with Windows Boot Manager");
    }

    #[test]
    #[ignore = "needs administrator rights and a prepared test disk"]
    fn windows_fallback_on_a_real_disk() {
        let number: u32 = std::env::var("INOCRIUM_TEST_ESP_DISK").unwrap().parse().unwrap();
        assert!(number > 3, "tests never touch disks 0-3");
        let disk = winio::RawDisk::open(number, false).unwrap();
        let expect = std::env::var("INOCRIUM_TEST_ESP_EXPECT").unwrap();
        let outcome = windows_boot(&disk).unwrap();
        println!("disk {number}: {outcome:?}");
        match (expect.as_str(), outcome) {
            ("patches", WindowsBoot::Patches(patches)) => {
                let (fallback, windows) = loaders(&disk);
                assert_ne!(fallback, windows);
                let (fallback, windows) = loaders(&Overlay { inner: &disk, patches });
                assert_eq!(fallback, windows, "the patched fallback loader must be Windows Boot Manager");
            }
            ("already", WindowsBoot::AlreadyDefault) => {
                let (fallback, windows) = loaders(&disk);
                assert_eq!(fallback, windows);
            }
            (expect, outcome) => panic!("expected {expect}, got {outcome:?}"),
        }
    }
}
