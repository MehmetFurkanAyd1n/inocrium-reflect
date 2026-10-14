# iscsi

In-process, thread-per-connection iSCSI target backed by `blockdev::BlockDevice`,
plus Windows Microsoft iSCSI Initiator control. Licensed GPL-3.0-only with the
workspace.

The target exports consecutive LUN numbers, uses an ephemeral listener when the
configured port is zero, and supports normal and SendTargets discovery sessions.
CHAP is one-way MD5 with a fresh random challenge on each connection. Use
`generate_chap_secret()` for Microsoft-compatible credentials and bind to
`127.0.0.1:0` for image mounting.

## Public API

```rust,ignore
pub struct LunConfig {
    pub device: Arc<dyn BlockDevice>,
    pub vendor: String,        // ASCII, <= 8 bytes
    pub product: String,       // ASCII, <= 16 bytes
    pub revision: String,      // ASCII, <= 4 bytes
    pub serial: String,        // ASCII, <= 252 bytes
    pub device_id: [u8; 16],    // NAA-6; first nibble is normalized to 6
}
pub struct ChapSecret { pub user: String, pub secret: Vec<u8> }
pub struct TargetConfig {
    pub bind: SocketAddr,
    pub iqn: String,
    pub chap: Option<ChapSecret>,
    pub luns: Vec<LunConfig>,
}
pub fn generate_chap_secret() -> ChapSecret;
pub fn iqn_for(id: &str) -> String;
impl TargetServer {
    pub fn start(cfg: TargetConfig) -> io::Result<TargetServer>;
    pub fn local_addr(&self) -> SocketAddr;
    pub fn iqn(&self) -> &str;
    pub fn stop(&mut self);
}
```

Windows-only `initiator` functions return the crate `Result<T>` and report native
codes with `Error::Win32 { operation, code }`:

```rust,ignore
pub fn ensure_initiator_service() -> Result<()>;
pub fn add_portal(addr: SocketAddr, chap: Option<&ChapSecret>) -> Result<()>;
pub fn remove_portal(addr: SocketAddr) -> Result<()>;
pub fn login(iqn: &str, addr: SocketAddr, chap: Option<&ChapSecret>) -> Result<Session>;
pub fn session_disks(s: &Session, timeout: Duration) -> Result<Vec<u32>>;
pub fn logout(s: &Session) -> Result<()>;
pub fn cleanup_stale(iqn_prefix: &str) -> Result<()>;
pub struct Session {
    pub session_id: [u64; 2],
    pub connection_id: [u64; 2],
}
```

`Session` implements `Clone`, `Send`, `Sync`, serde serialization and
deserialization. Logins are not persistent. Stale cleanup uses live session
connections and cached discovery records to attribute `127.0.0.1` portals to an
IQN prefix, and retains portals shared by other targets. An orphan portal with
no remaining target association is retained because its ownership cannot be
established.

## Protocol scope

No header/data digest, mutual CHAP, multi-connection sessions, additional header
segments, recovery levels above zero, or persistent reservations. Reservations
are cleanly rejected with ILLEGAL REQUEST. Task management completes as a no-op.
UNMAP is accepted without changing backing storage and provisioning VPD does not
advertise deallocation. Reads/writes support 6/10/12/16-byte CDBs. FUA writes and
SYNCHRONIZE CACHE call the backing device's `flush` method.

The target advertises a maximum 16 MiB SCSI transfer, a 256 KiB receive segment
and a maximum 256 KiB burst. Initiator receive segments and negotiated burst
limits govern outgoing data. Up to 256 LUNs and 32 queued commands are supported;
commands execute sequentially per connection. `stop` interrupts blocked socket
I/O and joins connection threads, but a backing-device call itself must return
before its thread can be joined.

## Verification

Run from the workspace with `CARGO_TARGET_DIR=target/agent-iscsi`:

```text
cargo fmt -p iscsi
cargo clippy -p iscsi --all-targets -- -D warnings
cargo test -p iscsi
```

Tests cover framing/text parsing, a separately calculated CHAP digest, discovery,
CHAP success/failure, inquiry/VPD, large-device capacity, all four read/write CDB
sizes, both InitialR2T modes, disabled ImmediateData, multiple data segments and
R2T bursts, pipelined commands, sense/error mapping, MODE SENSE write protection,
flush, UNMAP, reservations, NOP, task management, reject, logout, sequence
wraparound, simultaneous connections, multiple LUN reporting, and shutdown with
a partial header.

The Microsoft initiator end-to-end test is ignored by default because it needs
administrator rights and the MSiSCSI service. From an elevated shell run:

```text
cargo test -p iscsi windows_initiator_end_to_end -- --ignored
```

It uses a CHAP-protected 64 MiB memory disk and cleans up its session and portal
even if disk enumeration fails. Microsoft-specific negotiation, physical disk
arrival, service startup and stale cleanup still need live elevated validation;
the non-elevated test suite checks the Windows bindings and credential layouts
without invoking these operations.

The crate directly enables `windows` 0.58 features `Win32_Foundation`,
`Win32_Storage_IscsiDisc`, `Win32_System_Ioctl`, `Win32_System_Services` and
`Win32_Security`. `Win32_System_Services` is additional to the root feature set.
