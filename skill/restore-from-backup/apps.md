# App locations and caveats

Reference for the `restore-from-backup` skill. The paths are given in two forms:

- **Old**: relative to the old profile on the mounted system volume. For example, `<old>` = `E:\Users\inosh`.
- **New**: in the current Windows session (`$env:USERPROFILE`, `$env:APPDATA`, `$env:LOCALAPPDATA`).

Old paths may not match exactly. Check that an old path exists with `Test-Path -LiteralPath` before you rely on it. Entries marked **(uncertain)** are best knowledge, not verified on this machine.

Robocopy `/XD` and `/XF` accept either bare names, which match at any depth, or full paths. Use full paths built from the source folder for generic names such as `Cache`, so that an extension's own `Cache` folder is not skipped. Bare names are fine for unique names such as `GPUCache`.

---

## Chromium browsers: Google Chrome, Microsoft Edge, Brave, Vivaldi

| Browser | User Data folder (old: under `<old>\AppData\Local\`, new: under `$env:LOCALAPPDATA\`) | Processes |
| --- | --- | --- |
| Chrome | `Google\Chrome\User Data` | `chrome` |
| Edge | `Microsoft\Edge\User Data` | `msedge` (often running in the background, see below) |
| Brave | `BraveSoftware\Brave-Browser\User Data` | `brave` |
| Vivaldi | `Vivaldi\User Data` | `vivaldi` |

### Layout of `User Data`

- **`Local State`** (a JSON file): this is browser-wide state. It includes:
  - `profile.info_cache`, the list of profiles: directory name → display name, avatar, signed-in account (`user_name`);
  - `os_crypt.encrypted_key`, the AES key that encrypts passwords, cookies and cards. It is wrapped with the **Windows user's DPAPI**;
  - since Chrome 127, also `os_crypt.app_bound_encrypted_key`. This is "app-bound encryption": the key is wrapped by Chrome's elevation service with **SYSTEM DPAPI** and the user's DPAPI, and is tied to the machine and the Chrome install;
  - also the variations seed, metrics IDs and other machine-specific state.
- **Profile folders**: `Default`, `Profile 1`, `Profile 2`, and so on. Skip `Guest Profile` and `System Profile`. Read display names from the old `Local State`; it is plain JSON and readable from the mount:
  ```powershell
  $ls = Get-Content -LiteralPath "$oldUD\Local State" -Raw -Encoding UTF8 | ConvertFrom-Json
  $ls.profile.info_cache.PSObject.Properties | ForEach-Object { '{0} = {1} ({2})' -f $_.Name, $_.Value.name, $_.Value.user_name }
  ```
- **`Last Version`** (a text file in `User Data`): the browser version that last used this data.

### What is in a profile folder, and whether it carries over to a different Windows account

| Data | File(s) | After a reset (different account) |
| --- | --- | --- |
| Bookmarks | `Bookmarks`, `Bookmarks.bak` | ✅ yes |
| Bookmarks of the signed-in Google account | `AccountBookmarks` (`EncryptedAccountBookmarks2` is encrypted) | ⚠️ convert to HTML and import; see "Account bookmarks" below |
| History, typed URLs, downloads list | `History`, `Top Sites`, `Visited Links` | ✅ yes |
| Open tabs / last session | `Sessions\`, `Current Session`, `Current Tabs`, … | ✅ yes. The image was taken while Chrome was running, so Chrome may offer "Restore pages". |
| Extensions and their local data | `Extensions\`, `Local Extension Settings\`, `Extension State\`, `IndexedDB\`, `Local Storage\` | ✅ mostly. (uncertain) Chrome may disable some extensions or reset some settings; see "Secure Preferences" below. |
| Settings | `Preferences` | ✅ yes |
| Search engines, autofill **addresses**, autocomplete | `Web Data` | ✅ yes |
| Saved **passwords** | `Login Data`, `Login Data For Account` | ❌ no, they are encrypted with the old key |
| **Cookies** (site logins and sessions) | `Network\Cookies` | ❌ no, encrypted with the old key; app-bound (v20) since Chrome 127 |
| **Payment cards** (full numbers), IBANs | in `Web Data` | ❌ no, encrypted |
| Google account sign-in (sync token) | encrypted in `Web Data` | ❌ no; the user signs in again |
| Site settings, zoom levels, permissions | `Preferences` | ✅ yes |

### Account bookmarks

Recent Chrome versions keep the bookmarks of a signed-in Google account in `AccountBookmarks`, apart from the local `Bookmarks`. The local file may be nearly empty while the user's real bookmarks are all in `AccountBookmarks`, so check both and count their entries before telling the user what the backup holds. Chrome ties `AccountBookmarks` to the account and can discard it when the profile is not signed in to that account, so copying the file is not reliable (uncertain how each version handles it).

Convert it instead, and have the user import the HTML (`chrome://bookmarks` → ⋮ → *Import bookmarks*, or Edge `edge://favorites` → *Import favorites*). The script reads only the source and refuses to overwrite its destination:

```powershell
$convert = '<skill-dir>\scripts\ConvertTo-BookmarksHtml.ps1'
& $convert -Path "$oldProfile\AccountBookmarks" -Destination "$env:USERPROFILE\Desktop\chrome-account-bookmarks.html"
```

It prints the number of bookmarks written; tell the user that number. If the user had bookmark sync, signing in may bring them back anyway; importing as well can create duplicates, so ask first. Do the same for the local `Bookmarks` if the user wants the HTML route rather than a profile copy.

### The encryption caveat (explain this to the user)

Chromium browsers encrypt saved passwords, cookies (which hold logins and sessions) and payment cards with an AES key stored in `Local State`. That key is protected by the **old Windows user's DPAPI**. Since Chrome 127 there is also app-bound encryption, which uses Chrome's elevation service and **SYSTEM DPAPI** of the old installation; it was applied first to cookies, and Google is extending it to other data (uncertain). A Windows reset creates a new user with a **new SID and new DPAPI master keys**, even if the user name and password are the same, and a new machine DPAPI key. So the old key cannot be decrypted, and neither can anything encrypted with it.

Copying a profile therefore restores **bookmarks, history, extensions, settings, open tabs/sessions, autofill addresses and search engines**. It does **not** restore **passwords, cookies or site logins, payment cards, or the browser sign-in**.

The only ways to get the passwords back are:

1. **Browser sync** (Chrome Sync or Google Password Manager, Edge sync with a Microsoft account, Brave Sync). Sign in on the new system and they come back. This only works if password sync was on before the reset.
2. **A CSV export made before the reset** (`chrome://password-manager/settings` → Export, or Edge `edge://wallet/passwords` → Export). Import it on the same page. Afterwards, tell the user to delete the CSV: it is plain text.
3. Offline DPAPI recovery from the image, using the old Windows password, the old `Protect` master keys and, for v20, the old SYSTEM and SECURITY hives. This is forensic work. It is **not supported by this skill**; do not attempt it. You may mention that it exists.

Sites will also ask the user to log in again, because the cookies are lost.

### Keep the new `Local State` (the recommendation)

When the old and new SIDs differ (the normal case after a reset), **do not copy the old `Local State`**. Keep the one the new browser created.

- The only thing the old `Local State` adds is the profile list and names. Everything users care about lives in the profile folders.
- Its `os_crypt` keys cannot be decrypted by the new user or machine. Current Chromium code generates a new key when DPAPI decryption fails. The app-bound key and newer async key providers add failure modes that are less predictable (uncertain). In the best case you gain nothing; in the worst case you get errors or a profile that keeps failing to decrypt.
- It also carries machine-specific state: the variations seed, metrics client ID, the `Last Version` logic, and GPU and crash state.
- If the browser was never started on the new system, start it once and close it. That creates a fresh `Local State` with a working key, and a `Default` profile.

To get the profile list back without editing `Local State`:

- **One profile (usually `Default`)**: copy the old `Default` into the new `Default`, after setting the new one aside.
- **Several profiles**: have the user create the same number of profiles in the new browser first (profile menu → Add). Each one gets a `Profile N` folder registered in the new `Local State`. Close the browser, then copy each old profile folder's contents into the matching new folder. Map them by the old display names, which you read from the old `Local State`, and confirm the mapping with the user. Numbering does not have to match: old `Profile 3` can go into new `Profile 1`. The picker may show the new default name and avatar until the user renames the profile (uncertain whether it picks up the old name from `Preferences`).
- Merging `profile.info_cache` into the new `Local State` with a script is possible. It is **not** done by default, because hand-editing that file risks breaking the browser for little gain.

**Same account (the old and new SIDs are equal):** this means the image is of this same Windows installation, for example a user rolling back their own profile. DPAPI still works. Copy the whole `User Data`, including `Local State`, `Login Data` and cookies, after setting the current `User Data` aside. Use the user-data-level exclusions below.

### Exclusions

Per profile folder `$src` (full paths), these are caches and are rebuilt automatically:

```powershell
$xd = @('Cache', 'Code Cache', 'GPUCache', 'DawnCache', 'DawnGraphiteCache', 'DawnWebGPUCache',
        'Service Worker\CacheStorage', 'Service Worker\ScriptCache', 'blob_storage', 'GrShaderCache',
        'ShaderCache', 'GraphiteDawnCache') | ForEach-Object { Join-Path $src $_ }
```

These are `User Data`-level folders. Exclude them only when copying the whole `User Data` in the same-account case:

`GrShaderCache`, `GraphiteDawnCache`, `ShaderCache`, `Crashpad`, `component_crx_cache`, `extensions_crx_cache`, `BrowserMetrics`, `Webstore Downloads`, `optimization_guide_model_store`, `Safe Browsing`. These are caches, crash dumps, metrics and downloaded components, and the browser downloads again whatever it needs.

When the SIDs differ, also exclude files that cannot be decrypted. They would only be junk, and Chrome may delete undecryptable passwords anyway. The originals stay in the image.

```powershell
$xf = @('Login Data', 'Login Data-journal', 'Login Data For Account', 'Login Data For Account-journal',
        'Cookies', 'Cookies-journal')
```

### Before copying

- **Update the browser first** (`chrome://settings/help`, `edge://settings/help`). Its version must be **greater than or equal to** the old `User Data\Last Version`. Profile databases written by a newer version may fail to open in an older one. The installed version is the `Application\<version>` folder name, or the file version of `chrome.exe`, `msedge.exe` or `brave.exe`.
- **The browser must be fully closed.** Check processes. Chrome may keep running in the background ("Continue running background apps"): exit it from the tray icon. **Edge** almost always has `msedge` processes because of Startup boost and background extensions. Ask the user to turn off `edge://settings/system` → *Startup boost* and *Continue running background extensions and apps*, then close Edge. Alternatively, ask permission to stop `msedge` processes. `msedgewebview2` processes belong to other apps and do not lock Edge's profile; leave them alone.
- **Edge with a Microsoft account**: a new Windows signed in with an MSA may already have Edge signed in and syncing in `Default`. If the user had Edge sync, signing in may be all they need. Ask first.
- If the user had **sync** (Chrome or Edge): copy the profile first, then start the browser and sign in. Sync merges, and duplicates are mostly avoided (uncertain for bookmarks with recent "account bookmarks" changes). Alternatively, skip the copy and rely on sync; that is the user's choice.

### After copying

- Chrome may show "Some settings were reset" and reset the startup pages, homepage or default search engine. It may also disable some extensions until the user re-enables or repairs them. The cause is that `Secure Preferences` is protected with a MAC tied to the old machine (uncertain how many settings are affected).
- The profile will be signed out or "paused". Sign in again to resume sync.
- Import the password CSV if the user has one.

### Opera / Opera GX (uncertain layout; inspect first)

- Old: `<old>\AppData\Roaming\Opera Software\Opera Stable` (Opera GX: `Opera GX Stable`). This folder historically holds `Local State` **and** the default profile's files directly, with no `Default` subfolder. Newer Opera One builds may differ. Check where `Bookmarks` lives before copying.
- Caches are in `<old>\AppData\Local\Opera Software\Opera Stable`. Do not copy them.
- Same encryption caveat. If the profile files sit next to `Local State`, copy the folder **excluding** `Local State` (set the new folder aside first, then copy the new `Local State` back into place), or copy only the profile files.

---

## Mozilla Firefox

- Old: `<old>\AppData\Roaming\Mozilla\Firefox`. Contains:
  - `profiles.ini` (the profile list; `Path=Profiles/xxxxxxxx.default-release`, `IsRelative=1`);
  - `installs.ini` (maps an **install hash**, derived from the Firefox install directory, to the default profile);
  - `Profiles\<name>\`.
- `<old>\AppData\Local\Mozilla\Firefox\Profiles\<name>` contains **only caches** (`cache2`, `startupCache`, …). Do not copy it.
- New: `$env:APPDATA\Mozilla\Firefox`. The Microsoft Store version uses `$env:LOCALAPPDATA\Packages\Mozilla.Firefox_n80bbvh6b1yt2\LocalCache\Roaming\Mozilla\Firefox` (uncertain; check).
- Process: `firefox`.

**Firefox data is not tied to Windows DPAPI**, so almost everything transfers:

- Passwords (`logins.json` + `key4.db`) **do transfer**. If the user had set a **Primary Password**, they need that same Primary Password to unlock them in the new Firefox.
- Cookies (`cookies.sqlite`) transfer, so many site logins survive. Some sites still end the session because the device changed.
- Bookmarks and history (`places.sqlite`, `favicons.sqlite`), sessions (`sessionstore.jsonlz4`, `sessionstore-backups\`), extensions (`extensions\`, `extensions.json`, `storage\`), settings (`prefs.js`), certificates (`cert9.db`), form history and site permissions all transfer.
- The Windows-specific setting "Require device sign-in to fill passwords" is only a prompt, not encryption.

Recommended procedure, when Firefox on the new system holds nothing important:

1. Update Firefox. Its version must be ≥ `LastVersion` in the old profile's `compatibility.ini`. Otherwise Firefox warns about a downgrade and refuses the profile.
2. Close Firefox (no `firefox` processes).
3. If `$env:APPDATA\Mozilla\Firefox` exists, rename it to `Firefox.before-restore-<ts>`.
4. Copy the whole old `Roaming\Mozilla\Firefox` folder, excluding `Crash Reports`, `Pending Pings` and `minidumps`.
5. Start Firefox. If it opens an empty profile, the install hash differs (Firefox is installed in a different folder). Open `about:profiles` and use **Set as default profile** on the restored profile, then restart.

If the user wants to keep the new Firefox data, copy only the chosen old profile folder into `Profiles\` and register it with `about:profiles` → *Create a New Profile* → *Choose Folder*. That is an advanced, manual step.

---

## User folders (Desktop, Documents, Downloads, Pictures, Music, Videos)

- Old: `<old>\Desktop`, `<old>\Documents`, `<old>\Downloads`, `<old>\Pictures`, `<old>\Music`, `<old>\Videos`. Optionally also `Favorites`, `Links`, `Saved Games`, `Contacts`.
- On disk the folder names are **English** even on a Turkish Windows. Explorer shows "Masaüstü", "Belgeler", "İndirilenler", "Resimler", "Müzik", "Videolar" through `desktop.ini`.
- New destinations: read them, do not assume them. They may be redirected to OneDrive:
  ```powershell
  [Environment]::GetFolderPath('Desktop'); [Environment]::GetFolderPath('MyDocuments')
  [Environment]::GetFolderPath('MyPictures'); [Environment]::GetFolderPath('MyMusic'); [Environment]::GetFolderPath('MyVideos')
  (New-Object -ComObject Shell.Application).NameSpace('shell:Downloads').Self.Path
  ```
- **OneDrive-redirected old folders.** If `<old>\OneDrive*\Desktop`, `Documents` or `Pictures` exist and hold the user's files while `<old>\Desktop` is nearly empty, the old system used OneDrive folder backup ("Known Folder Move"). The OneDrive folder may be named `OneDrive - <Organization>`, and its subfolders may have localized names (uncertain). Those files are **already in OneDrive**.
  - Recommend signing in to OneDrive on the new PC instead of copying them.
  - Files that were "online-only" are cloud placeholders and **have no content in the image**. Robocopy will report errors for them, or skip them.
  - **Never copy into the new OneDrive folder** while OneDrive is syncing. That creates conflict copies (`file-PCNAME.ext`) and duplicates. If the user wants a local copy anyway, copy into a separate folder such as `<Documents>\Restored from backup\OneDrive`.
- If a **new** destination is inside OneDrive, copying into it uploads the files. Tell the user (cloud quota, upload time) before copying.
- Merge rule: copy **only new files** by default (`/XC /XN /XO`). List conflicts first, and ask before overwriting anything.
- `/XJ` is mandatory. `Documents` can contain legacy junctions (`My Music`, `My Pictures`, `My Videos`), and junction targets point to the **live** `C:`, not to the mounted volume.
- Large folders: check the size and the free space first.
- PowerShell profiles live in `Documents\WindowsPowerShell` and `Documents\PowerShell`, so they come along with Documents.

---

## Visual Studio Code

- Settings: `<old>\AppData\Roaming\Code\User\` → `$env:APPDATA\Code\User\`. Copy:
  - `settings.json`, `keybindings.json`, `tasks.json`, `mcp.json` (if present);
  - `snippets\`;
  - `profiles\` (named VS Code profiles).
- Skip:
  - `workspaceStorage\` (per-workspace state keyed by path);
  - `History\` (local file history; optional);
  - `sync\`;
  - `globalStorage\`. It holds extension state, and **secrets** (GitHub/Microsoft sign-in, extension tokens) are encrypted with Electron safeStorage, which uses DPAPI, so they do not transfer. The user signs in again. Copy `globalStorage` only if they ask.
- Extensions: `<old>\.vscode\extensions`. **Recommended:** reinstall them from the list instead of copying the folder, because `extensions.json` stores absolute paths with the old user name (uncertain how VS Code handles a different user name):
  ```powershell
  $ext = "$old\.vscode\extensions\extensions.json"
  (Get-Content -LiteralPath $ext -Raw | ConvertFrom-Json).identifier.id | Sort-Object -Unique
  # then, with the user's agreement: code --install-extension <id>
  ```
  If `extensions.json` is missing, folder names are `<publisher>.<name>-<version>`.
- If the user used **Settings Sync** (signed in with GitHub or Microsoft), signing in restores settings and extensions. Prefer that.
- VS Code Insiders: `Code - Insiders` and `.vscode-insiders`. Cursor: `Roaming\Cursor\User` and `.cursor\extensions` (same idea; uncertain details).
- Process: `Code` (Insiders: `Code - Insiders`).

---

## Windows Terminal

- Store version: `<old>\AppData\Local\Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json`. Preview: `Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe`. Unpackaged: `<old>\AppData\Local\Microsoft\Windows Terminal\settings.json`.
- Copy `settings.json` only, not `state.json`. Set the existing file aside first.
- Check it for absolute paths with the old user name (icons, `startingDirectory`, background images) and tell the user. Do not rewrite them without asking.
- The terminal reloads settings when the file changes. Close it anyway (process `WindowsTerminal`) to avoid a write race. If you run commands from Windows Terminal yourself, tell the user to restart it afterwards.

---

## SSH keys and Git

- `.ssh`: `<old>\.ssh` → `$env:USERPROFILE\.ssh` (`id_*`, `config`, `known_hosts`). Private keys are plain files and **do transfer**.
  - Robocopy `/COPY:DAT` does not copy ACLs, so the files inherit the profile's ACL (user, SYSTEM, Administrators), which Windows OpenSSH accepts. **Never copy security** from the mount: the mount grants Everyone full control.
  - Keys that were added to the Windows `ssh-agent` service are stored DPAPI-protected in the registry and do **not** transfer. Run `ssh-add` again.
  - Check `config` for `IdentityFile` paths with the old user name.
- `.gitconfig`: `<old>\.gitconfig` → `$env:USERPROFILE\.gitconfig`. If one already exists, show a diff and ask before replacing it. Paths inside it may name the old user.
- Git credentials in **Git Credential Manager** live in Windows Credential Manager and do not transfer; the user authenticates again. A plain `.git-credentials` file does transfer. It contains tokens in plain text; mention that.
- Process check: none needed, but make sure no `ssh-agent` or Git operation is in progress.

---

## Notepad++

- `<old>\AppData\Roaming\Notepad++\` → `$env:APPDATA\Notepad++\`. Contains `config.xml`, `shortcuts.xml`, `stylers.xml`, `langs.xml`, `contextMenu.xml`, `userDefineLangs\`, `themes\`, `plugins\config\`, `session.xml` and `backup\`.
- `backup\` holds **unsaved tabs**. `session.xml` refers to them, and to open files, by **absolute path**. If the user name changed, those paths point to the old profile. Tell the user; offer to show them, and do not edit them silently.
- Plugin binaries are under `Program Files\Notepad++\plugins`. Reinstall them with Plugins Admin.
- Notepad++ writes `config.xml` and `session.xml` on exit, so it **must be closed** (process `notepad++`), or it will overwrite the restored files.
- A portable or "local configuration" install (`doLocalConf.xml` next to `notepad++.exe`) keeps its settings in the install folder instead (uncertain whether that applies here).

---

## Things that do NOT carry over (tell the user)

- **Chromium browser passwords, cookies and site logins, payment cards, and browser sign-in** (Chrome, Edge, Brave, Opera, Vivaldi). See the caveat above.
- **Wi-Fi passwords**: they are protected by the old machine's DPAPI. They could only have been exported from the running old system (`netsh wlan export profile key=clear`).
- **Windows Credential Manager** entries: saved network, RDP and app credentials, and Git Credential Manager tokens.
- **Windows Hello** (PIN, face, fingerprint), **EFS**-encrypted files (unreadable even on the mount), and **certificates' private keys** in the user store.
- Logins of **Electron apps** (Teams, Slack, Discord, Spotify, VS Code accounts, and so on). They use safeStorage, which uses DPAPI; sign in again. **Signal Desktop's** database key is also protected this way, so copying its folder will not work (uncertain for older versions).
- **Installed programs.** The image holds their files, but not a working installation; reinstall them. App licences and activations are often machine-bound.
- **Outlook**: `.pst` files are plain files and transfer; `.ost` files are caches that re-download; account passwords do not transfer.
