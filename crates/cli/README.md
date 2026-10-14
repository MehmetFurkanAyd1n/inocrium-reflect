# inocrium CLI

The CLI connects to the named-pipe service first and falls back to the in-process engine when the service is unavailable. Direct operations require an elevated Windows console. `--direct` forces that mode. viBoot, jobs, history and logs require the service.

Command surface:

```text
inocrium [--direct] [--json] [--password-stdin] [--detach] <command>
disks
backup --disk N [--disk N] [--partitions 1,3] --dest PATH [backup options]
verify IMG
mount IMG [--writable] [--disk N] [--partition N]
unmount UUID|all
restore IMG --target-disk N [--disk N] [--partition N [--target-partition N | --target-offset BYTES]] [--extend] [--yes]
export-vhdx IMG OUT [--disk N]
redeploy --disk N [--drivers PATH ...]
viboot start IMG [--disk N] [--name NAME] [--memory MB] [--cpus N]
viboot list
viboot stop NAME
def list
def run UUID
def create --name NAME --disk N --dest PATH [backup options] [schedule options]
def delete UUID
jobs
jobs cancel UUID
history [--limit N]
logs JOB_UUID
```

Backup options: `--prefix NAME`, `--compress 0..19` (default 3), `--encrypt`, `--verify`, `--forensic`, `--split SIZE`, `--comment TEXT`, and `--keep-last N` or `--keep-days N`. Sizes use binary K/M/G/T suffixes; `4G` is 4294967296 bytes. Partition selection requires exactly one source disk. Image disk indexes default to 0 for restore, export and viBoot; mount defaults to all image disks, or disk 0 when a partition is supplied.

Schedule options: `--daily HH:MM`, `--weekly HH:MM --days mon,fri`, or `--monthly HH:MM --day 15`, optionally `--run-missed`. Weekday numbers are Sunday=0 through Saturday=6.

Passwords come from an echo-disabled console prompt, one stdin line with `--password-stdin`, or `INOCRIUM_PASSWORD`. New encrypted backups/definitions confirm interactively. No password-valued command-line argument exists. Image/RPC authentication gets at most three attempts; rejected supplied passwords require an interactive console to retry. `def run` over IPC uses the service's stored password: the existing `definitions.run` protocol has only an `id` parameter.

Restore asks for the target disk number unless `--yes` is supplied. Direct mounts remain alive until Ctrl+C in their owning process; direct `unmount` cannot control another process. Writable changes are discarded at unmount. IPC mount requests have a 180-second timeout.

Direct definitions use the service's `StoredDefinition` format at `%ProgramData%\inocrium\definitions\<uuid>.json`: flattened camelCase `Definition` fields, optional `passwordBlob` (hex DPAPI blob), and optional RFC3339 `anchor`. Unknown fields are accepted. Lists expose only `Definition` fields and derive `passwordSet` from the blob's presence; they never expose the blob. Direct creation protects the data directory with `D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`, writes and syncs a temporary file, then publishes it atomically without overwriting an existing file. This create-only policy preserves any existing `passwordBlob` and `anchor` byte-for-byte. New files have an anchor and no blob. Direct create/delete refuse while the service is running; omit `--direct` to use the service. Stop the service before offline definition changes and restart it afterwards to load them. Encrypted creation requires the service.

The service protects passwords with user-scope DPAPI as LocalSystem. An elevated user's direct process cannot decrypt those blobs and never attempts to do so. An encrypted direct `def run` explains this limitation and obtains the image password through the existing console/stdin/environment password input for that run, regardless of `passwordSet`. Remote `def run` sends only `{id}` and relies on the service to resolve its stored password, without a CLI password prompt or password retry.

Saved disk selections resolve exactly as in the service: MBR signatures are eight uppercase hex digits; identity comparisons ignore ASCII case and surrounding braces. A matching identity at the saved number wins, otherwise a unique match at another number is used. A missing or ambiguous stored identity fails instead of selecting whatever now occupies the old number. Selections without an identity use the disk number. Empty partition lists, absent selected partitions and duplicate resolved disks fail; selected partition numbers are sorted and deduplicated.

## Exact JSON output contract

`--json` writes one compact JSON value per stdout line. Arrays remain arrays even with zero or one element. UUIDs, paths and timestamps are strings. Optional fields serialize as `null`; object key order is unspecified. Progress and diagnostics go to stderr, so stdout must be parsed separately.

The following shapes describe every command's successful stdout:

- `disks`: array of Disk objects.
- `backup`, `verify`, `restore`, `export-vhdx`, `redeploy`, `viboot start`, `def run` **in service mode**: first `{"jobId":"UUID"}`; when following, a second line containing the terminal Job object. `--detach` emits only the first line.
- `backup`, `def run` **in direct mode**: `{"image":"PATH"}`.
- `verify` **in direct mode**: `{"verified":true,"image":"PATH"}`.
- `mount` **in either mode**: Mount object, emitted once when mounted.
- `unmount` **in service mode**: `{"unmounted":true}`. Direct mode returns an error.
- `restore` **in direct mode**: `{"target_disk":N,"partition":N_OR_NULL,"bytes_written":N,"identity_rewritten":BOOL,"extended":BOOL,"warnings":["TEXT"]}`. These field names deliberately match the engine's snake_case report.
- `export-vhdx` **in direct mode**: `{"output":"PATH","bytesWritten":N}`.
- `redeploy` **in direct mode**: `{"disk":N,"registryChanges":N,"driversAdded":["INF_PATH"],"bootRepairSkipped":"REASON_OR_NULL"}`. `bootRepairSkipped` is a string or JSON `null`, not a boolean. Multiple driver folders produce one aggregate result.
- `viboot list`: array of `{"name":"NAME","state":"STATE","image":"PATH"}` objects.
- `viboot stop`: `{"stopped":"NAME"}`.
- `def list`: array of Definition objects.
- `def create`: a JSON UUID string, `"UUID"`, in either mode.
- `def delete`: `{"deleted":"UUID"}` in either mode.
- `jobs`: array of Job objects.
- `jobs cancel`: `{"cancelRequested":"UUID"}`.
- `history`: array of HistoryEntry objects.
- `logs`: a JSON string containing the log text, with escaped newlines.

Object fields (all keys listed, including nullable fields):

```text
Disk:
  number, model, serial, size, sectorSize, style, diskId, isSystem, partitions
  style = "gpt" | "mbr" | "raw"
  partitions = Partition[]
Partition:
  number, offset, length, typeName, fileSystem, label, letter, usedBytes,
  isSystem, isBoot, isEsp
  fileSystem, label, letter, usedBytes may be null
Mount:
  id, image, disk, partition, writable, diskNumber, letters
  partition and diskNumber may be null; letters is string[]
Definition:
  id, name, enabled, disks, destination, prefix, mode, compressionLevel,
  encrypt, passwordSet, verify, splitSize, comment, retention, schedule,
  runMissed, lastRun, nextRun
  mode = "smart" | "forensic"
  disks = {number, diskId, partitions: number[] | null}[]
  retention = {kind: "keepLast" | "keepDays", value} | null
  schedule = {kind: "daily" | "weekly" | "monthly", time, weekdays: number[], dayOfMonth} | null
  splitSize, dayOfMonth, lastRun, nextRun may be null
Job:
  id, kind, title, state, progress, started, finished, error
  kind = "backup" | "verify" | "restore" | "exportVhdx" | "redeploy" | "viboot"
  state = "queued" | "running" | "succeeded" | "failed" | "cancelled"
  progress = {phase, detail, bytesDone, bytesTotal, bytesWritten, rate}
  started, finished, error may be null
HistoryEntry:
  jobId, kind, title, state, started, finished, bytes, error
  kind and state use the Job enums; finished and error may be null
```

JSON progress on stderr, in **both modes**, is:

```json
{"type":"progress","percent":50.0,"progress":{"phase":"backup","detail":"TEXT","bytes_done":512,"bytes_total":1024,"bytes_written":256,"rate":512}}
```

Progress field names are snake_case because the renderer uses `engine::Progress`; the final service Job's nested progress uses camelCase. MRIMGX verification supplies block counts in `detail` and the actual block percentage in `percent`; byte counters/rate are zero because its API reports blocks only. Stderr may also contain human-readable prompts, fallback notices, warnings or job errors. Runtime errors emit `{"error":"MESSAGE"}` on stderr. Clap help/argument errors use clap's normal text output. The non-Windows binary prints `inocrium reflect requires Windows` and exits 1.

Followed jobs exit 0 for succeeded, 1 for failed, and 130 for cancelled. Ctrl+C sends `jobs.cancel` once. Direct engine cancellation exits 130 with no success result; Ctrl+C closes a direct mount normally and exits 0.

## Validation and remaining integration work

```bash
export PATH="$HOME/.cargo/bin:$PATH"
rustfmt --edition 2021 crates/cli/src/direct.rs crates/cli/src/remote.rs
CARGO_TARGET_DIR=target/agent-cli cargo clippy -p inocrium --all-targets -- -D warnings
CARGO_TARGET_DIR=target/agent-cli cargo test -p inocrium
```

Unit tests cover clap commands, size/partition parsing, schedule flags, disk-selection validation and identity resolution, atomic create-only flattened definition persistence, preservation of service metadata, definition mutation policy, local named-pipe password retry limits, password-free remote definition runs, terminal job exit codes, and cancellation notifications. Physical-disk enumeration is an ignored admin test. Real backup/restore, protected ProgramData ACLs, LocalSystem DPAPI, VSS, iSCSI mount, ReDeploy and Hyper-V/service integration need the separate administrative fixture workflow; they are not exercised by the unit tests. See `tests/README.md` for the VHDX suite and its non-admin syntax/helper validators.
