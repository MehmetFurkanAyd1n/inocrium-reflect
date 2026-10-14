# Administrator VHDX end-to-end tests

These Pester tests exercise the built `inocrium.exe` against disposable virtual disks.
They do not build the CLI. Every scenario creates a separate workspace at
`$env:TEMP\inocrium-tests\<guid>` and registers resources before provisioning them.
Never run against real disks: the helpers resolve each generated VHDX through
`Get-DiskImage | Get-Disk`, recheck its disk number, and reject boot/system disks.

## Prerequisites

- Windows with **Administrator** PowerShell, Windows PowerShell 5.1 or PowerShell 7.
- Pester 5 or newer:
  `Install-Module Pester -MinimumVersion 5 -Scope CurrentUser`.
- A Windows build of `inocrium.exe`, normally created with
  `cargo build --release -p inocrium`.
- Windows Storage cmdlets (`Get-DiskImage`, `Get-Disk`, `Initialize-Disk`,
  `New-Partition`, `Format-Volume`, etc.) and VSS support for NTFS backup.
- Prefer the Hyper-V PowerShell module (`New-VHD`, `Mount-VHD`, `Dismount-VHD`).
  If those commands are unavailable, the helpers use Windows `diskpart.exe`
  with generated, validated VHDX paths. The fallback does not require Hyper-V.
- Mount scenarios need a working Microsoft iSCSI initiator and the CLI's image
  mount implementation. A running `InocriumReflect` service is tried first;
  otherwise a background `--direct` CLI process holds the image mount.
- Allow several GiB of free space in TEMP, unused drive letters, and time for
  backup/restore, compression, encryption, and hashing. All scenarios are tagged
  `Admin` and `Slow`. Do not run this suite in parallel or change disks/drive
  letters during a run.

## Run

From an elevated PowerShell in the repository root:

```powershell
.\tests\Run-AdminTests.ps1
.\tests\Run-AdminTests.ps1 -InocriumExe 'C:\build output\inocrium.exe' -Tag Admin
.\tests\Run-AdminTests.ps1 -Tag Slow -ExcludeTag SomeFutureTag
```

The runner prefers `target\release\inocrium.exe`, then `target\debug\inocrium.exe`.
It rejects a non-admin session with exit code 1, checks Pester >= 5, uses detailed
output, and returns a nonzero exit code on a test/container/cleanup failure.
Skipped mount tests are visible in the report; a successful exit with skips does
not establish that mounting works.

To select a specific scenario, use a Pester container in an elevated session:

```powershell
Import-Module Pester -MinimumVersion 5.0
$container = New-PesterContainer -Path .\tests\Vhdx.EndToEnd.Tests.ps1 -Data @{
    InocriumExe = (Resolve-Path .\target\release\inocrium.exe).Path
}
Invoke-Pester -Container $container -FullNameFilter '*forensic*' -Output Detailed
```

Syntax validation is safe in a **non-admin** session:

```powershell
powershell.exe -NoProfile -File .\tests\Validate-Syntax.ps1
# If installed, also run the parser in PowerShell 7:
pwsh -NoProfile -File .\tests\Validate-Syntax.ps1
# Safe fake-CLI helper checks; does not invoke inocrium.exe or Storage cmdlets:
powershell.exe -NoProfile -File .\tests\Validate-Helpers.ps1
```

The validator parses every `.ps1`/`.psm1` under tests without executing the admin
suite. It also runs PSScriptAnalyzer when installed; missing analyzer is reported.
`Validate-Helpers.ps1` compiles a temporary fake console CLI and checks native
argument quoting, child-only passwords, JSON/JSON-lines parsing, graceful Ctrl+C,
stdout forwarding, and workspace preservation after injected cleanup failures.
It requires the Windows PowerShell .NET Framework compiler; it needs no Pester.

## Coverage

| Scenario | Observable checks |
| --- | --- |
| GPT smart backup / verify / restore | Dynamic 1 GiB source, 64 MiB FAT32 then NTFS, random SHA256 payload including 50 MiB file; 2 GiB restore with `--extend`, preserved layouts/hashes, increased NTFS partition and volume size |
| MBR forensic round trip | Same-size 256 MiB target, `--forensic`, verify, layout and SHA256 comparisons |
| Encrypted split image | Incompressible payload >64 MiB, `.irimg` plus `.i01`/later parts; correct password succeeds, wrong password fails, correct password still succeeds |
| Corruption | Successful initial verify, 32 bytes flipped midway through the image, subsequent verify fails |
| Read-only mount | Payload hashes, a write throws and creates no file, unmount removes access |
| Writable mount | Write/read overlay probe, unmount, read-only remount shows probe absent and original hashes |
| VHDX export | Output exists, attach read-only, both filesystems/layouts and payload hashes preserved |
| Retention | Three backups with one prefix and `--keep-last 2`, exactly newest two primaries remain and verify |
| Partition-only restore | `--partitions` backup, `--disk 0 --partition N --target-partition M`; restored hashes, neighbor hashes, target layout unchanged |

## CLI contract and reconciliation

`Invoke-Inocrium` is the single native invocation helper. It separately captures
stdout, stderr, and exit code, drains both pipes asynchronously, applies a finite
timeout, and quotes Windows arguments (including paths with spaces). It prefixes
commands with global `--json --direct`; service mount/unmount omit `--direct`.
Service mount also passes `--detach`: the current CLI rejects direct fallback
with that flag, preventing an unreachable service from creating an untracked,
indefinitely running direct mount. Mount itself still returns its mount object.
Ordinary commands must finish with exit code 0 on success and nonzero on failure.
Passwords are synthetic and injected only into the child process through
`INOCRIUM_PASSWORD`; a caller's existing password environment variable is neither
used nor modified, and passwords are never command arguments.

Image discovery uses files on disk, not backup JSON fields. An image is expected
to be `<prefix>-<timestamp>.irimg`, split parts share that base with `.i01`, `.i02`,
etc., and successful backup removes `<img>.ckpt`. Timestamp granularity is assumed
to be no worse than one second for the retention fixture. Source partition
numbers and target disk numbers come from Windows; image disk selection is
assumed to use the image-local index `0`. Partition numbers are assumed to match
the source Windows partition table even after `--partitions` selection.

The current `output.rs` prints compact JSON to **stdout**. `remote.rs` and
`direct.rs` both print a mount object with `id` (UUID), `image`, `disk`,
`partition`, `writable`, `diskNumber`, and `letters` (such as `["X:"]`). The
helper reads `letters` and accepts legacy `drive_letter`/`driveLetter` and
`mount_points`/`mountPoints` fields; identity aliases are `id`, `mount_id`, and
`mountId`. It requires one unambiguous nonempty identity. The fallback diffs
`Get-Volume` and requires the scenario's unique payload directory.

Direct backup prints `{"image":"..."}`; verify prints `{"verified":true,
"image":"..."}`. Remote job commands print `{"jobId":"..."}`, then the
final job object when followed. JSON progress and errors go to **stderr**, along
with some plain diagnostics. The parser accepts a JSON document or the last
structured JSON line, ignoring trailing plain text/scalars. Image discovery
and integrity assertions use actual files and exit codes, so job fields are
not assumed. A successful service mount without a recognizable id or a service
mount timeout preserves its workspace and fails clearly for manual recovery.

Mounts unavailable through both service and direct mode are skipped with
`Set-ItResult -Skipped` and diagnostic reasons. Hash/write/overlay assertion
failures after a mount is available fail the test. Partition-only restore is
provisioned on a larger disk with an existing sufficiently large destination;
failure is reported rather than silently skipped.

## Cleanup and recovery

`AfterEach` detaches registered VHDXs and removes that scenario's workspace;
`AfterAll` retries every workspace even after setup or assertion failures.
Service mounts are unmounted by their own ids. Direct mount processes are
stopped with Ctrl+C through `Direct-MountHost.ps1`, which owns a private hidden
console. This lets `direct.rs` run `manager.unmount` and discard COW files;
force-killing it would bypass that code. No other process receives the signal.
The host's shutdown is bounded by the parent; a CLI that fails to exit preserves
the process and workspace for recovery. Live iSCSI teardown still needs admin
acceptance; the non-admin helper check only proves the console transport.
The suite never calls `unmount all`. Unconfirmed detach/unmount failures preserve
the workspace and fail cleanup so that an attached image is not deleted. Reparse
points and paths outside the generated GUID workspace are rejected.

A killed PowerShell session cannot run Pester cleanup. If interrupted externally,
inspect the reported workspace, detach its VHDXs with `Dismount-VHD -Path ...`
or a `diskpart` `select vdisk file="..."` / `detach vdisk` script, stop the exact
background CLI process if needed, and unmount any reported service mount id.
For a live private-console host, creating its `mount-host-*.json.stop` marker
in the preserved workspace requests Ctrl+C without force-killing the child.
For an unrecognized service JSON response, identify that image's mount id through
the application's mount list before unmounting it. Remove the workspace only
after confirming those resources are detached.
