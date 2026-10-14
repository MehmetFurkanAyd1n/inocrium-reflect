//! Mounting images as local disks through a loopback iSCSI target.
//!
//! Each mount runs its own target on `127.0.0.1` (ephemeral port, random CHAP secret) with
//! one LUN per image disk, and logs the Microsoft initiator in to it. Writable mounts put a
//! copy-on-write overlay in front of the image; the overlay file is deleted at unmount, so
//! changes are discarded.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use blockdev::{BlockDevice, CowOverlay, ImageDiskView, ViewOptions};
use chrono::{DateTime, Utc};
use iscsi::initiator::{self, Session};
use iscsi::{LunConfig, TargetConfig, TargetServer};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use winio::RawDisk;

use super::restore::{read_head, retable, Patched};
use super::AnyImage;
use crate::{Error, Result};

/// IQN prefix of every target this process creates (see [`iscsi::iqn_for`]).
pub const IQN_PREFIX: &str = "iqn.2026-01.org.inocrium:image-";

const DISK_TIMEOUT: Duration = Duration::from_secs(30);
const VOLUME_TIMEOUT: Duration = Duration::from_secs(20);
/// Pause between attempts to detach a disk that another program holds open.
const RELEASE_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MountRequest {
    pub image: PathBuf,
    pub password: Option<String>,
    /// Allow writes; they go to a temporary copy-on-write file and are discarded at unmount.
    pub writable: bool,
    /// Image disk to mount; all disks when `None`.
    pub disk: Option<u32>,
    /// Only expose (irimg) or only report (mrimgx) this partition of `disk` (default 0).
    pub partition: Option<u32>,
    /// Give volumes without a drive letter a free one.
    pub assign_letters: bool,
    /// Expose NTFS permissions as imaged. Otherwise every file and folder is readable and
    /// writable by everyone (see [`crate::ntfs`]); the image itself is not changed.
    pub keep_permissions: bool,
}

impl std::fmt::Debug for MountRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountRequest")
            .field("image", &self.image)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("writable", &self.writable)
            .field("disk", &self.disk)
            .field("partition", &self.partition)
            .field("assign_letters", &self.assign_letters)
            .field("keep_permissions", &self.keep_permissions)
            .finish()
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn request_debug_redacts_password() {
        let req = MountRequest { password: Some("private mount password".into()), ..Default::default() };
        let debug = format!("{req:?}");
        assert!(!debug.contains("private mount password"));
        assert!(debug.contains("<redacted>"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountedVolume {
    pub image_disk: u32,
    /// Local disk number of the mounted image disk.
    pub disk: u32,
    pub partition: u32,
    pub guid_path: String,
    /// Drive letters (`"E:"`), possibly assigned by the mount.
    pub mount_points: Vec<String>,
    pub filesystem: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub id: String,
    pub image: PathBuf,
    pub writable: bool,
    /// Local disk numbers, in image disk order.
    pub disks: Vec<u32>,
    pub volumes: Vec<MountedVolume>,
    pub cow_paths: Vec<PathBuf>,
    pub created: DateTime<Utc>,
    pub warnings: Vec<String>,
}

/// One active mount and everything needed to tear it down.
struct Mount {
    info: MountInfo,
    server: Option<TargetServer>,
    portal: Option<SocketAddr>,
    session: Option<Session>,
    /// Drive letters this mount assigned.
    assigned: Vec<String>,
}

impl Mount {
    /// Best-effort teardown; returns the problems encountered and whether a program still
    /// holds a disk of the session open. Then the target keeps running and teardown is to be
    /// repeated: stopping it would leave Windows reconnecting a session with no target. With
    /// `force` the target is stopped regardless.
    fn teardown(&mut self, force: bool) -> (Vec<String>, bool) {
        let mut problems = Vec::new();
        for m in self.assigned.drain(..) {
            if let Err(e) = winio::remove_mount_point(&m) {
                problems.push(format!("removing {m}: {e}"));
            }
        }
        // Dismount first so file systems are not surprise-removed with open handles.
        for &n in &self.info.disks {
            if let Ok(raw) = RawDisk::open(n, true) {
                let locks = raw.lock_and_dismount_volumes().unwrap_or_default();
                let _ = raw.set_attributes(Some(true), None);
                drop(locks);
            }
        }
        if let Some(s) = self.session.take() {
            match initiator::logout(&s) {
                Ok(()) => {}
                Err(iscsi::Error::Busy(e)) if !force => {
                    self.session = Some(s);
                    problems.push(format!("iSCSI logout: {e}"));
                    return (problems, true);
                }
                Err(e) => problems.push(format!("iSCSI logout: {e}")),
            }
        }
        if let Some(addr) = self.portal.take() {
            if let Err(e) = initiator::remove_portal(addr) {
                problems.push(format!("removing iSCSI portal: {e}"));
            }
        }
        if let Some(mut server) = self.server.take() {
            server.stop();
        }
        for p in &self.info.cow_paths {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    problems.push(format!("deleting {}: {e}", p.display()));
                }
            }
        }
        (problems, false)
    }
}

pub struct MountManager {
    mounts: Mutex<HashMap<String, Mount>>,
    /// Threads finishing unmounts whose disk another program held open, and their images.
    releasing: Mutex<Vec<(JoinHandle<()>, PathBuf)>>,
    /// Tells those threads to give up waiting.
    stop_releasing: Arc<AtomicBool>,
    cow_dir: PathBuf,
}

impl MountManager {
    /// `cow_dir` holds the copy-on-write files of writable mounts.
    pub fn new(cow_dir: impl Into<PathBuf>) -> Self {
        Self {
            mounts: Mutex::new(HashMap::new()),
            releasing: Mutex::new(Vec::new()),
            stop_releasing: Arc::new(AtomicBool::new(false)),
            cow_dir: cow_dir.into(),
        }
    }

    /// Removes what a previous run left behind: sessions and portals of our targets and
    /// copy-on-write files. Call once before the first mount.
    pub fn startup_cleanup(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = initiator::cleanup_stale(IQN_PREFIX) {
            problems.push(format!("cleaning up iSCSI sessions: {e}"));
        }
        if let Ok(dir) = std::fs::read_dir(&self.cow_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cow")) {
                    if let Err(e) = std::fs::remove_file(&path) {
                        problems.push(format!("deleting {}: {e}", path.display()));
                    }
                }
            }
        }
        problems
    }

    pub fn list(&self) -> Vec<MountInfo> {
        let mut out: Vec<MountInfo> = self.mounts.lock().values().map(|m| m.info.clone()).collect();
        out.sort_by_key(|m| m.created);
        out
    }

    pub fn mount(&self, req: &MountRequest) -> Result<MountInfo> {
        let image = AnyImage::open(&req.image, req.password.as_deref())?;
        let id = Uuid::new_v4().simple().to_string();
        let mut mount = Mount {
            info: MountInfo {
                id: id.clone(),
                image: req.image.clone(),
                writable: req.writable,
                disks: Vec::new(),
                volumes: Vec::new(),
                cow_paths: Vec::new(),
                created: Utc::now(),
                warnings: Vec::new(),
            },
            server: None,
            portal: None,
            session: None,
            assigned: Vec::new(),
        };
        match self.attach(&image, req, &mut mount) {
            Ok(()) => {
                let info = mount.info.clone();
                self.mounts.lock().insert(id, mount);
                Ok(info)
            }
            Err(e) => {
                mount.teardown(true);
                Err(e)
            }
        }
    }

    fn attach(&self, image: &AnyImage, req: &MountRequest, mount: &mut Mount) -> Result<()> {
        let image_disks: Vec<u32> = match (req.disk, req.partition) {
            (Some(d), _) => vec![d],
            (None, Some(_)) => vec![0],
            (None, None) => (0..image.disk_count() as u32).collect(),
        };
        if image_disks.is_empty() {
            return Err(Error::invalid("the image contains no disks"));
        }
        let id = mount.info.id.clone();

        let mut luns = Vec::new();
        // Partition offset (on the exposed disk) to report, per LUN.
        let mut only: Vec<Option<u64>> = Vec::new();
        for (lun, &d) in image_disks.iter().enumerate() {
            let src = image.disk(d).ok_or_else(|| Error::invalid(format!("the image has no disk {d}")))?;
            let (device, offset): (Arc<dyn BlockDevice>, Option<u64>) = match image {
                AnyImage::Irimg(r) => match req.partition {
                    Some(p) => {
                        let view = ImageDiskView::partition(r.clone(), d, p, ViewOptions::default())?;
                        let offset = view.partition_offset(p);
                        (Arc::new(view), offset)
                    }
                    None => (Arc::new(ImageDiskView::disk(r.clone(), d, ViewOptions::default())?), None),
                },
                AnyImage::Mrimgx(m) => {
                    let dev = m.disk_device(d as usize)?;
                    // A fresh identity, so the mount does not collide with the source disk.
                    let patches = retable(&read_head(&*dev)?, src.sector_size, src.size, src.size, true, false).writes;
                    let offset = match req.partition {
                        Some(p) => Some(
                            m.info().disks[d as usize]
                                .partitions
                                .iter()
                                .find(|x| x.number == p)
                                .map(|x| x.offset)
                                .ok_or_else(|| Error::invalid(format!("disk {d} of the image has no partition {p}")))?,
                        ),
                        None => None,
                    };
                    (Arc::new(Patched::new(dev, patches)), offset)
                }
            };
            let device: Arc<dyn BlockDevice> = if req.keep_permissions {
                device
            } else {
                let (patches, warnings) = crate::ntfs::open_permissions(&*device);
                mount.info.warnings.extend(warnings);
                if patches.is_empty() {
                    device
                } else {
                    Arc::new(Patched::new(device, patches))
                }
            };
            let device: Arc<dyn BlockDevice> = if req.writable {
                std::fs::create_dir_all(&self.cow_dir)?;
                let path = self.cow_dir.join(format!("{id}-{d}.cow"));
                mount.info.cow_paths.push(path.clone());
                Arc::new(CowOverlay::create(device, &path)?)
            } else {
                device
            };
            let mut device_id = *Uuid::new_v4().as_bytes();
            device_id[0] = 0x60 | (device_id[0] & 0x0F); // NAA-6
            luns.push(LunConfig {
                device,
                vendor: "INOCRIUM".into(),
                product: "Image Disk".into(),
                revision: "1.0".into(),
                serial: format!("{id}-{lun}"),
                device_id,
            });
            only.push(offset);
        }

        let chap = iscsi::generate_chap_secret();
        let iqn = iscsi::iqn_for(&id);
        debug_assert!(iqn.starts_with(IQN_PREFIX));
        let server = TargetServer::start(TargetConfig {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            iqn: iqn.clone(),
            chap: Some(chap.clone()),
            luns,
        })?;
        let addr = server.local_addr();
        mount.server = Some(server);

        initiator::ensure_initiator_service()?;
        initiator::add_portal(addr, Some(&chap))?;
        mount.portal = Some(addr);
        let session = initiator::login(&iqn, addr, Some(&chap))?;
        mount.session = Some(session.clone());
        let found = initiator::session_disks(&session, DISK_TIMEOUT)?;

        // Match local disks to LUNs by the serial number we gave each LUN.
        let mut disks = vec![None; image_disks.len()];
        let mut unmatched = Vec::new();
        for n in found {
            let serial = winio::disk_info(n).map(|i| i.serial).unwrap_or_default();
            match (0..image_disks.len()).find(|lun| serial.trim() == format!("{id}-{lun}")) {
                Some(lun) => disks[lun] = Some(n),
                None => unmatched.push(n),
            }
        }
        unmatched.sort_unstable();
        let mut unmatched = unmatched.into_iter();
        let disks: Vec<u32> = disks
            .into_iter()
            .map(|d| d.or_else(|| unmatched.next()))
            .collect::<Option<_>>()
            .ok_or_else(|| Error::invalid("the mounted image disks did not appear"))?;
        mount.info.disks = disks.clone();

        for &n in &disks {
            let raw = RawDisk::open(n, true)?;
            if !req.writable {
                let _ = raw.set_attributes(None, Some(true));
            }
            raw.set_attributes(Some(false), None)?;
            let _ = raw.update_properties();
        }

        for (lun, &n) in disks.iter().enumerate() {
            if winio::wait_for_disk_volumes(n, VOLUME_TIMEOUT).is_err() {
                mount.info.warnings.push(format!("no volumes appeared on image disk {}", image_disks[lun]));
                continue;
            }
            for p in winio::disk_info(n)?.layout.partitions {
                if only[lun].is_some_and(|o| o != p.offset) {
                    continue;
                }
                let Some(v) = p.volume else { continue };
                let mut mount_points: Vec<String> = v.drive_letters.iter().map(|c| format!("{c}:")).collect();
                if mount_points.is_empty() && req.assign_letters && !v.filesystem.is_empty() {
                    match winio::free_drive_letter() {
                        Some(c) => {
                            let m = format!("{c}:");
                            match winio::set_mount_point(&m, &v.guid_path) {
                                Ok(()) => {
                                    mount.assigned.push(m.clone());
                                    mount_points.push(m);
                                }
                                Err(e) => mount.info.warnings.push(format!("assigning {m}: {e}")),
                            }
                        }
                        None => mount.info.warnings.push("no free drive letter".into()),
                    }
                }
                mount.info.volumes.push(MountedVolume {
                    image_disk: image_disks[lun],
                    disk: n,
                    partition: p.number,
                    guid_path: v.guid_path,
                    mount_points,
                    filesystem: v.filesystem,
                    label: v.label,
                });
            }
        }
        Ok(())
    }

    /// Unmounts `id`, discarding writes. Returns problems that did not stop the unmount.
    ///
    /// The drive letters and volumes are gone on return. If a program (such as Task Manager's
    /// Performance page) still holds the disk open, the disk itself is detached in the
    /// background once it is released.
    pub fn unmount(&self, id: &str) -> Result<Vec<String>> {
        let mut mount =
            self.mounts.lock().remove(id).ok_or_else(|| Error::invalid(format!("no mount with id {id}")))?;
        let (mut problems, busy) = mount.teardown(false);
        if busy {
            problems.push(format!(
                "the image disk stays attached, offline, until the program holding it open lets go; it is \
                 detached automatically then (mount {id})"
            ));
            self.release_later(mount);
        }
        Ok(problems)
    }

    /// Unmounts everything, including disks still waiting to be released, which are then
    /// detached even though a program holds them open.
    pub fn unmount_all(&self) -> Vec<String> {
        let all: Vec<Mount> = self.mounts.lock().drain().map(|(_, m)| m).collect();
        let problems = all.into_iter().flat_map(|mut m| m.teardown(true).0).collect();
        self.stop_releasing.store(true, Ordering::SeqCst);
        for (handle, _) in std::mem::take(&mut *self.releasing.lock()) {
            let _ = handle.join();
        }
        self.stop_releasing.store(false, Ordering::SeqCst);
        problems
    }

    /// Images still read by unmounted disks that wait to be released.
    pub fn releasing_images(&self) -> Vec<PathBuf> {
        let mut releasing = self.releasing.lock();
        releasing.retain(|(h, _)| !h.is_finished());
        releasing.iter().map(|(_, image)| image.clone()).collect()
    }

    fn release_later(&self, mut mount: Mount) {
        let image = mount.info.image.clone();
        let stop = self.stop_releasing.clone();
        let handle = std::thread::spawn(move || {
            let id = mount.info.id.clone();
            loop {
                let wait = Instant::now();
                while wait.elapsed() < RELEASE_RETRY && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let force = stop.load(Ordering::SeqCst);
                let (problems, busy) = mount.teardown(force);
                if !busy {
                    if force && !problems.is_empty() {
                        tracing::warn!("mount {id} detached while still in use: {}", problems.join("; "));
                    } else {
                        tracing::info!("mount {id}: the image disk was released and detached");
                    }
                    return;
                }
            }
        });
        let mut releasing = self.releasing.lock();
        releasing.retain(|(h, _)| !h.is_finished());
        releasing.push((handle, image));
    }

    pub fn cow_dir(&self) -> &Path {
        &self.cow_dir
    }
}

impl Drop for MountManager {
    fn drop(&mut self) {
        self.unmount_all();
    }
}
