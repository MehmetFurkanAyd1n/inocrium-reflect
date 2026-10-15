---
name: restore-from-backup
description: Restore files, app settings or browser data from an inocrium reflect backup image (*.irimg) into the current Windows user, typically after a Windows reset or reinstall. Covers Chrome, Edge, Brave and Firefox profiles (bookmarks, history, extensions, settings), Desktop/Documents/Downloads/Pictures, VS Code, Windows Terminal, .ssh, .gitconfig, Notepad++ and similar. Use when the user asks to copy, move, migrate, transfer or recover something "from my backup / image / old Windows", including Turkish requests such as "yedekten Chrome ayarlarımı / yer imlerimi bu sisteme taşı", "yedekten geri yükle", "eski Windows'taki dosyalarımı aktar". The skill mounts the image read-only with the installed inocrium CLI (UAC prompts) and copies with robocopy.
---

# Restore from an inocrium backup image

You help the user bring data from an inocrium backup image into the **current** Windows user. The image is mounted **read-only** through the installed inocrium service, and you copy files with robocopy. Per-app paths and caveats are in [apps.md](apps.md); read the relevant section before touching an app.

This skill's folder is `<skill-dir>`; inocrium reflect installs it next to `inocrium.exe`. `inocrium skill` prints this guide with that path filled in, `inocrium skill apps` prints [apps.md](apps.md), and `inocrium skill --path` prints only the folder. The elevation helper is `<skill-dir>\scripts\Invoke-InocriumElevated.ps1`.

## Hard rules

1. **Never write to the mounted image volume.** Mount without `--writable`. Read from it only.
2. **Never delete user data.** To replace a file or folder, first rename the existing one to `<name>.before-restore-<yyyyMMdd-HHmmss>` in the same place. Do not remove those backups afterwards. Tell the user they can delete them once they are happy. Delete only if the user explicitly asks and names the path.
3. **Write only inside the current user's profile** (`$env:USERPROFILE`, including `AppData`), or into a folder the user names. Do not touch other disks, the image file, partitions, or system folders. Never run inocrium `restore`, `backup`, `def` or `redeploy`; the helper refuses them anyway.
4. **Passwords:**
   - Never ask for, accept, repeat or store an image password (or any other password) in the chat, in files, in logs or on a command line.
   - For an encrypted image, run the helper with `-Interactive`. The user types the password into the elevated window.
   - If the user pastes a password into the chat anyway, do not use it. Tell them to treat it as exposed, and use `-Interactive`.
5. **Ask before overwriting** any file or folder that already exists in the destination. Do **one app at a time**.
6. **Do not stop processes without permission.** Ask the user to close the app. Stop a process only if the user explicitly allows it, and prefer a graceful close.
7. **Robocopy:**
   - Never use `/MIR`, `/PURGE`, `/MOV`, `/MOVE`, `/COPYALL`, `/SEC`, `/COPY` with `S`, `O` or `U`, `/B` or `/ZB`.
   - The mount has opened every NTFS permission to Everyone, so copying security would spread that ACL. `/COPY:DAT` lets copied files inherit the destination's ACL.
   - Always use `/XJ`. Junctions in the old profile point to the **live** `C:`.
8. **Every `mount` and `unmount` goes through the helper.** It runs elevated and asks for UAC. Only `inocrium skill`, `inocrium --version` and `inocrium --json info <image>` may be called directly: they read and need no elevation (`info` may first print "Service unavailable…" on stderr; that is normal for a non-elevated caller). Every other command fails unless the caller is elevated, and must not be used.
9. **Speak the user's language.** Use Turkish if the user writes Turkish. Keep commands, paths and app names verbatim. Before each UAC prompt, say that a Windows administrator prompt is about to appear and why.

## Running commands

Use the **PowerShell** tool. Each call is a new process, so redefine variables in every call. Give helper calls a **long timeout** (`600000` ms), because the user must approve UAC.

```powershell
$helper = '<skill-dir>\scripts\Invoke-InocriumElevated.ps1'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File $helper mount 'G:\backup-20261014-221351.irimg'
"exit=$LASTEXITCODE"
```

With `-File`, pass the inocrium arguments separated by **spaces**; commas do not work there. The helper adds `--json`. It prints inocrium's JSON on stdout and its own messages on stderr (prefixed `inocrium-elevated:`). Add `-DryRun` to validate a request without elevating.

| Helper exit code | Meaning |
| --- | --- |
| 0 | Success |
| 1 (or another inocrium code) | inocrium failed; read the `{"error": ...}` line on stderr |
| 64 | Invalid helper arguments; nothing ran |
| 65 | The image needs a password. Rerun with `-Interactive` (if already interactive: no password was given, or it was not accepted) |
| 66 | inocrium is not installed, or the `InocriumReflect` service is not running |
| 67 | inocrium timed out. The service may still finish; check the GUI's Mounts page |
| 68 | The elevated window ended without a response (for example, it was closed) |
| 1223 | The user cancelled UAC. Do **not** retry automatically; ask the user |

Keep a small state file so a long session can recover: `$env:TEMP\inocrium-restore\session.json` with the image path, mount `id`, `letters`, system volume, old profile path and whether the SIDs match. It must never hold a password. Robocopy logs go to `$env:TEMP\inocrium-restore\logs\`.

## Procedure

### 1. Understand the request

Work out which app or folder the user means, and which data (for Chrome: bookmarks, history, extensions, all of it?). If it is unclear, ask. Say up front what cannot come back for that app: see "The encryption caveat" in [apps.md](apps.md) for browsers.

### 2. Find the image

If the user gave a path, use it. Otherwise, search the local drives' top levels, and say you are doing so:

```powershell
# Fast: -Filter, depth 3, skipping the big system folders (-Include is ignored with -LiteralPath).
$skip = @($env:SystemRoot, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData, "$env:SystemDrive\Users")
$found = foreach ($drive in Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -match '^[A-Z]:\\$' }) {
  foreach ($ext in '*.irimg', '*.mrimgx') {
    Get-ChildItem -LiteralPath $drive.Root -Filter $ext -File -Force -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath $drive.Root -Directory -Force -ErrorAction SilentlyContinue | Where-Object { $skip -notcontains $_.FullName } |
      ForEach-Object { Get-ChildItem -LiteralPath $_.FullName -Filter $ext -Recurse -Depth 2 -File -Force -ErrorAction SilentlyContinue }
  }
}
$found | Sort-Object LastWriteTime -Descending | Select-Object FullName, LastWriteTime, @{n='GB';e={[math]::Round($_.Length/1GB,1)}}
```

- If nothing is found, also search `$env:USERPROFILE` (Downloads, Documents) and ask the user where they keep backups.
- File names are usually `<prefix>-<yyyyMMdd>-<HHmmss>.irimg`. Split images also have `.i01`, `.i02`, …; mount the `.irimg`.
- If several images match, list them and ask which one: usually the latest one taken **before** the reset.
- `inocrium --json info '<image>'` shows, without mounting or elevation, when the image was taken and which disks, partitions, file systems and sizes it holds. Use it to confirm the choice with the user, and to check that the image has an NTFS system partition before mounting. For an encrypted image it fails asking for a password; then go straight to mounting with `-Interactive`.
- The service runs as LocalSystem and cannot see per-user mapped network drives. Use a local path or a UNC path.

### 3. Mount read-only (UAC prompt #1)

1. Tell the user that a Windows administrator prompt is coming for mounting the backup read-only, and ask them to approve it.
2. Mount with `mount '<image>'`. The default mount opens NTFS permissions so you can read everything without elevation; do **not** add `--keep-permissions`.
3. On exit code 65, explain that the image is encrypted. Say that an elevated window will open, and that they should type the password **there, not in the chat**. Rerun with `-Interactive`.
4. Parse the JSON line, for example `{"id":"…","image":"…","disk":0,"partition":null,"writable":false,"diskNumber":4,"letters":["F","E","I"]}`. **Record the `id`.** The CLI has no "list mounts" command. The letters come **without a colon**; add one (`$letters = $m.letters | ForEach-Object { $_.TrimEnd(':') + ':' }`), because `"F\Windows"` would be a relative path.
5. If `letters` is empty, tell the user and look at the inocrium GUI's Mounts page. Do not guess.

### 4. Find the old Windows and the profiles

```powershell
$letters = @('F','E','I') | ForEach-Object { $_.TrimEnd(':') + ':' }   # from the mount JSON
$sys = $letters | Where-Object { (Test-Path -LiteralPath "$_\Windows\System32") -and (Test-Path -LiteralPath "$_\Users") } | Select-Object -First 1
Get-ChildItem -LiteralPath "$sys\Users" -Directory -Force |
  Where-Object { -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $_.Name -notin 'Public','Default','Default User','All Users' } |
  Select-Object Name, LastWriteTime
```

- If several old users exist, ask which one. The user's backup normally has `Users\inosh`.
- If no volume has `\Windows` and `\Users`, the volume may be BitLocker-locked or not NTFS. Stop and tell the user.
- The new profile is `$env:USERPROFILE`. The user name may differ from the old one.
- **Same account or not?**
  ```powershell
  $oldSid = (Get-ChildItem -LiteralPath "$old\AppData\Roaming\Microsoft\Protect" -Directory -Force | Where-Object Name -match '^S-1-5-21-').Name
  $newSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  ```
  - Different SIDs (the normal case after a reset): old DPAPI-protected data cannot be decrypted here.
  - Equal SIDs: this image is of the current installation, and DPAPI-protected data still works. [apps.md](apps.md) explains what changes in that case.
- Tell the user briefly what you found: the volume, the old user, and which of the requested apps have data in the image. Do not dump directory listings.
- Make sure your shell's working directory is **not** on the mounted drive.

### 5. Transfer one app at a time

For each app or folder:

1. **Confirm the scope** with the user: which profile(s), and which folders. For a browser, say clearly what will and will not come back. Ask whether they used sync, and whether they exported passwords before the reset.
2. **Prerequisites** from [apps.md](apps.md):
   - The app is installed and **updated** (version ≥ the old one).
   - It has been started once if that file says so (Chromium: to create a fresh `Local State`).
   - There is enough free space: compare `(Get-PSDrive C).Free` with the source size (measure it with `robocopy <src> <tmpdir> /L /E /XJ /BYTES /NJH /NFL /NDL` or `Get-ChildItem -Recurse -File -Force | Measure-Object Length -Sum`).
3. **The app must be closed:**
   ```powershell
   Get-Process -Name chrome -ErrorAction SilentlyContinue | Select-Object Id, ProcessName, MainWindowTitle
   ```
   If it is running, ask the user to close it fully and wait, then check again. Use process names from [apps.md](apps.md). Stop a process only with explicit permission: `CloseMainWindow()` first, `Stop-Process` only if the user agrees.
4. **Set aside what exists**, after telling the user exactly what will be renamed:
   ```powershell
   $ts = Get-Date -Format 'yyyyMMdd-HHmmss'
   if (Test-Path -LiteralPath $dst) { Rename-Item -LiteralPath $dst -NewName ((Split-Path -Leaf $dst) + ".before-restore-$ts") }
   ```
   For merges into folders that hold user files (Documents and similar), do **not** rename. Use the conflict check below instead.
5. **Conflict check** for merges. Robocopy output is localized, so use a path-only listing: `/XL` excludes files that do not yet exist in the destination, so it lists only files that would **replace** an existing one:
   ```powershell
   $logs = Join-Path $env:TEMP 'inocrium-restore\logs'; $null = New-Item -ItemType Directory -Force -Path $logs
   $plan = Join-Path $logs "$ts-<app>-conflicts.log"
   robocopy $src $dst /L /E /XJ /XL /R:0 /W:0 /NJH /NJS /NDL /NC /NS /FP /XD $xd /XF $xf /UNILOG:$plan | Out-Null
   $conflicts = @(Get-Content -LiteralPath $plan -Encoding Unicode | ForEach-Object Trim | Where-Object { $_ })
   ```
   If there are conflicts, show a short summary (count and a few examples) and ask. The options are:
   - (a) copy only new files (`/XC /XN /XO`, the default);
   - (b) overwrite after backing up the conflicting files: copy them to a `.before-restore-<ts>` folder first;
   - (c) copy everything into a separate folder such as `Restored from backup`.
6. **Copy:**
   ```powershell
   $log = Join-Path $logs "$ts-<app>.log"
   robocopy $src $dst /E /COPY:DAT /DCOPY:DAT /R:1 /W:1 /XJ /NP /XD $xd /XF $xf /UNILOG:$log | Out-Null
   $rc = $LASTEXITCODE
   ```
   - Build `$xd` (excluded directories) and `$xf` (excluded files) from [apps.md](apps.md). Use full paths for generic names. When a list is empty, leave out its `/XD` or `/XF` switch entirely.
   - To copy a single file: `robocopy <srcDir> <dstDir> <fileName> /COPY:DAT /R:1 /W:1 /NP /UNILOG:$log`.
   - Large copies can exceed the tool timeout. Use a long timeout, or run the copy in the background and monitor the log.
7. **Check the exit code.** Robocopy's code is a bit mask:
   - **0–7 means success.** 0 = nothing to copy, 1 = files copied, 2 = extra files exist in the destination (normal), 4 = mismatches.
   - **8 or more means failure.** 8 = some files failed, 16 = fatal error.
   - On ≥ 8, find the failed files in the log by their language-independent error codes, `Select-String -LiteralPath $log -Pattern '\(0x[0-9A-Fa-f]{8}\)' -Encoding Unicode`. Explain them: for example, EFS-encrypted files, online-only OneDrive files, or files locked in the destination. Never "fix" a failure with `/B` or by changing ACLs.
8. **Summarize** for the user:
   - what was copied, from where to where;
   - the totals from the summary block at the end of the log (its labels may be localized);
   - what was set aside, and its path;
   - the log path;
   - what the user should do next in the app (start it, sign in, import passwords, re-enable extensions…).

   Then ask whether they want the next app.

### 6. Be honest about what does not carry over

Before unmounting, tell the user plainly, in their language, about everything relevant from "Things that do NOT carry over" in [apps.md](apps.md). The most important: Chromium browser **passwords, cookies (site logins) and payment cards** cannot be recovered after a reset, unless they had sync or a CSV export. Wi-Fi passwords and Credential Manager entries do not transfer. Electron-app logins (Teams, Discord, …) need a new sign-in.

### 7. Unmount (UAC prompt #2)

- When the user says they are done, ask whether they want to copy anything else first. If they will continue later, offer to leave the image mounted. Mounts persist until unmounted, a service restart or a reboot.
- Before unmounting:
  - make sure no copy is running;
  - your shell is not on the drive;
  - ask the user to close Explorer windows on the mounted drive, and **Task Manager**: its disk page holds the disk open and makes the unmount fail.
- Announce the UAC prompt, then run `powershell.exe -NoProfile -ExecutionPolicy Bypass -File $helper unmount <id>`. Success prints `{"unmounted":true}`.
- If the disk is busy, current inocrium versions finish detaching in the background once it is released.
- **Lost the id?** Mounted disks have the serial `<mount id>-<LUN>` (current implementation; uncertain in future versions):
  ```powershell
  Get-CimInstance Win32_DiskDrive | Where-Object SerialNumber -match '^[0-9a-fA-F-]{36}-\d+$' | Select-Object Model, SerialNumber
  ```
  Otherwise use the GUI's Mounts page. Use `unmount all` only after the user confirms. It also removes mounts they made in the GUI.
- Finally, tell the user where the `.before-restore-*` backups and logs are, and remind them they can delete those once they are happy.

## Troubleshooting

- **Exit 66, service not running:** the user can start it from an elevated prompt (`Start-Service InocriumReflect`) or reboot. Do not run inocrium in direct mode: a direct mount lives only as long as its CLI process.
- **Exit 1223:** the user declined UAC. Ask whether to try again; never loop.
- **The mount fails with an iSCSI error:** the Microsoft iSCSI Initiator service (`MSiSCSI`) must be able to start. Report the exact error to the user and suggest the inocrium GUI as a cross-check.
- **Access denied when reading the mount:** the image was mounted with `--keep-permissions`, or the files are EFS-encrypted. Remount without `--keep-permissions` (unmount first); EFS files cannot be read.
- **Turkish characters look garbled in console output:** that is console encoding only. Robocopy logs are UTF-16 (`/UNILOG`), so read them with `-Encoding Unicode`.
