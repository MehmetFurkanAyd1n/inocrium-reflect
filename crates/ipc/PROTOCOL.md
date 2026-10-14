# inocrium reflect IPC protocol v1

The service runs as LocalSystem. Clients use the local byte-mode named pipe
`\\.\pipe\inocrium-reflect`. Each message is UTF-8 JSON-RPC 2.0 framed as
`Content-Length: N\r\n\r\n<json>`, where N is the number of UTF-8 **bytes**.
This is compatible with StreamJsonRpc `HeaderDelimitedMessageHandler`.
Headers are limited to 8 KiB and payloads to 16 MiB. Header names are case
insensitive; exactly one Content-Length is required. Additional headers such
as Content-Type are accepted. Frames may arrive fragmented or concatenated.
Batch requests are not supported (InvalidRequest). Request IDs may be strings,
numbers or null; clients should use non-null integer IDs. Notifications omit ID.
Every request uses named/object params; empty params may be `{}`, null or omitted.
Results of methods returning no data are JSON null. IDs and job IDs are raw UUID
strings (not `{id: ...}` wrappers). All property and enum names are camelCase.
Unknown DTO properties are ignored for forward compatibility. Unknown methods
return MethodNotFound. Optional request fields may be omitted or null.

## Security and lifecycle

Production defaults use the protected SDDL
`D:P(A;;GA;;;BA)(A;;GA;;;SY)` (Administrators and SYSTEM only),
`PIPE_REJECT_REMOTE_CLIENTS`, and `FILE_FLAG_FIRST_PIPE_INSTANCE` for the first
instance. After reading the first frame, the server impersonates the client,
checks BUILTIN\Administrators membership, and reverts before any handler runs.
Rejected clients are disconnected without dispatch. UAC-filtered administrator
tokens are not sufficient: clients must be elevated. RevertToSelf failure is
fatal rather than continuing as a client. Client connections request
SecurityImpersonation, not delegation. Never log request bodies containing secrets.

The listener creates a new instance for each connection; each connection has a
reader/dispatch thread and a writer thread. Writes time out after 30 seconds.
Broadcasts queue without waiting for network/pipe writes; a full 64-message queue
disconnects the slow client. Reconnect and refresh lists to recover state.
The sync client has a 256-notification queue; overflow disconnects to avoid silently
losing state. No notification replay or automatic mutation retry is provided.
Clients must not retry `backup.start`, restore, save or other mutations blindly
after a timeout: a request can have completed even if its response was lost.

`ServerOptions { require_admin: false, sddl: ... }` is an explicit test-only
override; production must keep defaults. Tests use a unique pipe and an Owner
Rights ACE (OW) to permit the descriptor's creating user unelevated, never Everyone.
Shutdown cancels/drains overlapped I/O and joins workers. Synchronous handlers must
return promptly (enqueue jobs rather than doing disk imaging inside `call`). A
handler cannot be forcibly interrupted by this transport. Do not call shutdown
from inside a handler; shut down from the owning service thread.

## Rust API

- `proto`: all DTOs, `methods::*`, `notifications::*`, `PIPE_NAME`,
  `PROTOCOL_VERSION`, error constants and `RpcError`.
- `server::Handler::call(&self, method: &str, params: Value, client: &ClientInfo)
  -> Result<Value, RpcError>`. `ClientInfo` includes `process_id` and `is_admin`.
- `proto::Service`: one typed method per RPC, all with default MethodNotFound;
  each receives typed params and `&ClientInfo`. `proto::dispatch` deserializes
  params, calls the method, and serializes the result.
  `server::ServiceHandler<T>(T)` adapts a typed service to Handler.
- `Server::start(&str, Arc<dyn Handler>) -> io::Result<Server>`;
  `start_with_options(..., ServerOptions)`; `notifier()`; `shutdown(&self)`.
- Cheap cloneable `Notifier::broadcast<P: Serialize>(&self, method, params)
  -> Result<(), RpcError>`.
- `Client::connect(&str, Duration) -> Result<Client, client::Error>`;
  `call<P: Serialize, R: DeserializeOwned>(&self, method, params)` (30s);
  `call_with_timeout(..., Duration)`; `is_connected()`; `notifications()` returns
  a crossbeam Receiver of `{method, params}`. Receiver clones share a queue, so
  use one consumer. `is_service_running()` probes the default pipe;
  `is_pipe_running(name)` probes an alternate pipe. Existence does not mean
  authorization or successful handshake. `Error::AccessDenied` is distinct from
  `Disconnected`, `Timeout`, `Rpc`, `Io`, and `Json`.
- Transport modules are Windows-only; framing and DTOs work on other platforms.
- `Definition::validate()` provides basic range/schedule checks. Typed dispatch
  validates DTO shape; service implementations must call semantic validation,
  resolve disk identities, validate paths, check conflicts and enforce all safety
  rules before performing any operation.

Example:

```rust,no_run
use ipc::{proto::*, server::*};
use std::sync::Arc;
struct Api;
impl Service for Api {
    fn service_info(&self, _: EmptyParams, _: &ClientInfo) -> Result<ServiceInfo, RpcError> {
        Ok(ServiceInfo { version: "0.1.0".into(), protocol_version: PROTOCOL_VERSION, machine: "host".into() })
    }
}
# fn main() -> std::io::Result<()> {
let server = Server::start(PIPE_NAME, Arc::new(ServiceHandler(Api)))?;
server.notifier().broadcast(notifications::MOUNTS_CHANGED, EmptyParams {})
    .map_err(std::io::Error::other)?;
server.shutdown();
# Ok(()) }
```

## Errors

All errors are `{code: integer, message: string, data?: any}`. `data` must not
contain passwords. Standard codes: ParseError -32700, InvalidRequest -32600,
MethodNotFound -32601, InvalidParams -32602, InternalError -32603.
Application codes: Cancelled -32001, NotFound -32002, Busy -32003,
PasswordRequired -32004, BadPassword -32005, AccessDenied -32006.
AccessDenied from opening the pipe is a transport error, not an RPC response.
Handler panics become InternalError. All methods can return InvalidParams,
MethodNotFound, InternalError and AccessDenied. Specific expected errors are
listed below; implementations should use these codes rather than new ad hoc ones.
Long operations return a job ID immediately; later failures appear in job state,
history and logs. `jobs.cancel` requests cancellation; it does not imply immediate
completion. Destructive restore confirmation is a GUI responsibility, while the
service must independently validate target identity, size, exclusivity and safety.

## Shared DTOs

All byte counts, offsets and rates are unsigned 64-bit integers; rate is bytes/s.
Disk/partition numbers and indexes are unsigned 32-bit. UUIDs are standard strings.
Times are RFC3339 UTC strings; optional times are null until known. Drive letters
are strings such as `C` (without colon). Empty arrays are `[]`, not null.

- `ServiceInfo = {version, protocolVersion: u32, machine}`.
- `Disk = {number, model, serial, size, sectorSize, style: gpt|mbr|raw, diskId,
  isSystem, partitions: Partition[]}`. diskId is a GPT GUID or MBR signature
  string; services must compare it with current disk identity before a backup.
- `Partition = {number, offset, length, typeName, fileSystem: string|null,
  label: string|null, letter: string|null, usedBytes: u64|null, isSystem, isBoot, isEsp}`.
- `DiskSelection = {number, diskId, partitions: u32[]|null}`; null means whole disk,
  an empty array must not be interpreted as whole disk.
- `Definition = {id, name, enabled, disks: DiskSelection[], destination, prefix,
  mode: smart|forensic, compressionLevel: 0..19, encrypt, passwordSet, verify,
  splitSize: u64|null, comment, retention: Retention|null, schedule: Schedule|null,
  runMissed, lastRun: timestamp|null, nextRun: timestamp|null}`.
- `Retention = {kind: keepLast|keepDays, value: positive u32}`.
- `Schedule = {kind: daily|weekly|monthly, time: "HH:MM", weekdays: [0..6],
  dayOfMonth: 1..31|null}`. 0 = Sunday. Time is local to the service machine.
  Weekly requires weekdays; monthly requires dayOfMonth. The service defines DST
  handling and skips unavailable month days rather than silently changing targets.
- `Progress = {phase, detail, bytesDone, bytesTotal, bytesWritten, rate}`;
  bytesTotal=0 means indeterminate. BytesWritten is output bytes, not source bytes.
- `Job = {id, kind: backup|verify|restore|exportVhdx|redeploy|viboot, title,
  state: queued|running|succeeded|failed|cancelled, progress: Progress,
  started: timestamp|null, finished: timestamp|null, error: string|null}`.
- `ImageSummary = {path, created, comment, sizeOnDisk, encrypted,
  definitionId: UUID|null, disks: ImageDisk[], incomplete}`.
- `ImageDisk = {index, size, style: gpt|mbr|raw, partitions: ImagePartition[]}`.
- `ImagePartition = {number, length, fileSystem: string|null, label: string|null,
  captured, mode: smart|forensic}`. Only captured partitions can be mounted/restored.
- `ImageDetails` currently aliases ImageSummary; inspect returns authenticated
  metadata of this shape. Additive fields may be added in protocol v1.
- `Mount = {id, image, disk, partition: u32|null, writable,
  diskNumber: u32|null, letters: string[]}`. Writable mount changes use an overlay;
  the original backup remains immutable.
- `VirtualMachine = {name, state: string, image}` (Hyper-V state display string).
- `Settings = {defaultDestination, logRetentionDays: positive u32,
  threads: positive u32, ...}`; extra properties are retained in Rust `extra` and
  C# extension data. `settings.set` wraps this object under `settings`.
- `Credential = {target, user}`. Target is `\\server\share`, never a password.
- `HistoryEntry = {jobId, kind, title, state, started, finished: timestamp|null,
  bytes, error: string|null}`. kind/state use Job's enums.

## Methods

`{}` below means EmptyParams. `password?` is write-only and may be omitted/null.
Passwords must never appear in list/get/results, notifications, error data or logs.
Saved passwords belong to the service's DPAPI-protected store. `passwordSet` is
informational; the service recomputes it and does not trust the submitted flag.
For saves, a missing password retains an existing secret; disabling encrypt
clears it. A new encrypted definition requires a nonempty password.
Ad hoc backup.start uses a flattened Definition plus password; provide a fresh
UUID, null lastRun/nextRun and passwordSet=false for a new ad hoc backup. Schedules
and retention fields retain the same DTO shape; scheduling is not registered by
backup.start. Results are raw values as shown, including raw UUID strings.

| Method | Params | Result | Additional expected errors |
| --- | --- | --- | --- |
| service.info | `{}` | ServiceInfo | — |
| disks.list | `{}` | Disk[] | Busy |
| definitions.list | `{}` | Definition[] | — |
| definitions.get | `{id: UUID}` | Definition | NotFound |
| definitions.save | `{definition: Definition, password?}` | UUID (saved definition ID) | Busy, PasswordRequired, BadPassword |
| definitions.delete | `{id: UUID}` | null | NotFound, Busy |
| definitions.run | `{id: UUID}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| backup.start | `{...Definition, password?}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| jobs.list | `{}` | Job[] | — |
| jobs.cancel | `{id: UUID}` | null | NotFound, Busy |
| images.list | `{folders?: string[]}` | ImageSummary[] | NotFound |
| images.inspect | `{path: string, password?}` | ImageDetails | NotFound, PasswordRequired, BadPassword |
| images.verify | `{path: string, password?}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| images.delete | `{path: string}` | null | NotFound, Busy |
| mounts.list | `{}` | Mount[] | — |
| mounts.mount | `{path, password?, disk?: u32, partition?: u32, writable: bool}` | Mount | NotFound, Busy, PasswordRequired, BadPassword |
| mounts.unmount | `{id: UUID or "all"}` | null | NotFound, Busy |
| restore.start | `{path, password?, disk: u32, partition?: u32, targetDisk: u32, targetPartition?: u32, targetOffset?: u64, extend: bool}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| vhdx.export | `{path, password?, disk: u32, output: string}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| viboot.start | `{path, password?, disk: u32, vmName?: string, memoryMb: positive u32, cpus: positive u32}` | UUID (job ID) | NotFound, Busy, PasswordRequired, BadPassword |
| viboot.list | `{}` | VirtualMachine[] | — |
| viboot.stop | `{name: string}` | null | NotFound, Busy |
| redeploy.start | `{disk: u32, drivers?: string[]}` | UUID (job ID) | NotFound, Busy |
| settings.get | `{}` | Settings | — |
| settings.set | `{settings: Settings}` | null | Busy |
| credentials.list | `{}` | Credential[] | — |
| credentials.set | `{target: string, user: string, password: string}` | null | BadPassword |
| credentials.delete | `{target: string}` | null | NotFound, Busy |
| history.list | `{limit: u32}` | HistoryEntry[] (newest first) | — |
| logs.read | `{jobId: UUID}` | string (log text) | NotFound |

`images.list` with omitted/null folders uses service-configured search folders;
an empty explicit list searches no folders. Image disk indexes are indexes within
the image, not physical disk numbers. `mounts.mount` without disk selects the first
captured disk; without partition mounts the whole captured disk. Restore's source
disk is an image index, targetDisk is a physical disk number. Whole-disk restores
must omit targetPartition; partition restores may specify a target partition or
unallocated targetOffset. The service must reject ambiguous/overlapping targets,
unavailable captured extents and insufficient space. `extend` applies to supported
partition/filesystem resizing. Password/driver/path/resource validation belongs
to the service. `images.delete` must check mounts/jobs and delete the complete
split-image set, not an arbitrary unrelated file. Redeploy is reserved for the CLI
or future GUI workflow. Hyper-V features can return MethodNotFound on unsupported
hosts. Limit log payloads to the framing bound; log output must redact secrets.

## Server notifications

| Method | Params |
| --- | --- |
| job.progress | `{jobId: UUID, progress: Progress}` |
| job.state | `{job: Job}` |
| mounts.changed | `{}` |
| definitions.changed | `{}` |

Notifications carry no ID or response. The two changed signals invalidate cached
lists. Job progress is a snapshot, not a delta. On every reconnection, call
service.info and refresh state before relying on notifications.

## Validation commands

PowerShell equivalent of the requested shell PATH/target setup:

```powershell
$env:PATH = "$HOME\.cargo\bin;$env:PATH"
$env:CARGO_TARGET_DIR = "target/agent-ipc"
cargo fmt -p ipc
cargo clippy -p ipc --all-targets -- -D warnings
cargo test -p ipc
```

The regular tests include an unelevated unique-pipe loopback. An additional
cross-language integration test builds the actual GUI's RpcService and DTO files
into a console harness (no WinUI or elevation needed). It validates UTF-8,
application error codes, all notifications, and reconnect after service restart:

```powershell
dotnet build crates/ipc/tests/dotnet/ProtocolSmoke.csproj -c Release
cargo test -p ipc --test dotnet -- --ignored --nocapture
```

That test is ignored in the normal Rust suite because it requires the .NET SDK
and the prebuilt smoke project. All transport fixtures use unique pipes and
Owner Rights test ACLs; no installed service or physical disk is touched.
