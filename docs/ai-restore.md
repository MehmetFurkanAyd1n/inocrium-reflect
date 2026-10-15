# Restoring files and app settings with Claude Code

After a Windows reset or reinstall, you can ask [Claude Code](https://claude.com/claude-code) to copy things back from an inocrium backup image. For example: "move my Chrome bookmarks from my backup into this system". inocrium reflect ships a skill for this, [`skill/restore-from-backup`](../skill/restore-from-backup/SKILL.md): a guide plus helper scripts that the assistant follows with the installed inocrium tool. The installer puts it in `C:\Program Files\inocrium reflect\skill\restore-from-backup`, and the CLI prints it:

| Command | Prints |
| --- | --- |
| `inocrium skill` | The guide: how to find, mount and copy from an image, and the rules to follow, with the installed paths filled in |
| `inocrium skill apps` | Where apps keep their data, and what does not carry over after a reset |
| `inocrium skill --path` | Only the skill's folder |
| `inocrium --json info <image>` | What an image holds (disks, partitions, file systems, sizes), without mounting |

None of these needs administrator rights or the service, so any AI assistant that can run commands can read the guide from the console.

> **Early-stage software.** Keep your backup image until you have checked the restored data. The skill never changes the image, and it renames anything it would replace instead of deleting it.

## What the skill does

1. It finds your backup image (`*.irimg`) or asks you which one to use.
2. It mounts the image **read-only** through the inocrium service. You approve one Windows administrator (UAC) prompt.
3. It locates the old Windows volume and your old user folder in the image. It also checks whether the image belongs to the same Windows account; after a reset it does not.
4. It handles **one app or folder at a time**:
   - It confirms with you what to bring back.
   - It asks you to close the app.
   - It renames what already exists to `<name>.before-restore-<timestamp>`.
   - It copies with `robocopy`, skipping caches. It writes a log to `%TEMP%\inocrium-restore\logs`.
   - It reports what was copied and what failed.
5. It tells you what cannot be restored (see below).
6. When you are done, it unmounts the image after a second UAC prompt. If you want to copy more later, it can leave the image mounted.

Supported out of the box:

- Google Chrome, Microsoft Edge, Brave and Vivaldi profiles; Opera, but its layout is checked first;
- Mozilla Firefox;
- Desktop, Documents, Downloads, Pictures, Music and Videos, with OneDrive-redirected folders handled;
- VS Code settings and the extension list;
- Windows Terminal settings;
- `.ssh`, `.gitconfig`;
- Notepad++.

Other apps work if you tell Claude where their data lives. Per-app details are in [`apps.md`](../skill/restore-from-backup/apps.md).

Image passwords never go through the chat. If your image is encrypted, an administrator PowerShell window opens and you type the password there; it is passed only to `inocrium.exe` through an environment variable. Never paste a password into Claude Code.

## Setup after a reset

1. Install inocrium reflect from its MSI. The `InocriumReflect` service must be running; it starts automatically after installation.
2. Install Claude Code ([setup guide](https://docs.claude.com/en/docs/claude-code/setup)) and sign in.
3. Give the assistant the skill. Use either of these:
   - Start your request with: `Run "inocrium skill" and follow it.` (Turkish: `"inocrium skill" komutunu çalıştır ve ona göre ilerle.`). This works with any assistant that can run commands. The CLI is on `PATH` if you chose that installer option; otherwise it is `& "C:\Program Files\inocrium reflect\inocrium.exe" skill`.
   - Or install it as a Claude Code personal skill once, so Claude loads it by itself whenever you mention your backup:
     ```powershell
     $dir = New-Item -ItemType Directory -Force "$env:USERPROFILE\.claude\skills\restore-from-backup"
     inocrium skill | Set-Content -Encoding utf8 (Join-Path $dir 'SKILL.md')
     inocrium skill apps | Set-Content -Encoding utf8 (Join-Path $dir 'apps.md')
     ```
     This writes only the two guides, with the installed paths filled in. The scripts stay in Program Files, where only administrators can change them; that matters because the helper runs elevated. Run it again after upgrading inocrium reflect.
4. Run Claude Code as your normal user, **not** as administrator. The skill asks for elevation only for mounting and unmounting.
5. Make sure the disk that holds the backup is connected, and that the apps you want to restore are installed and up to date. A browser must be at least as new as the one that wrote the backup.

## Example prompts

Turkish:

- `Yedeğimdeki Chrome yer imlerimi ve ayarlarımı bu sisteme taşı.`
- `G:\backup-20261014-221351.irimg yedeğinden Masaüstü ve Belgeler klasörlerimi geri getir, mevcut dosyaların üzerine yazma.`
- `Eski Windows'taki Firefox profilimi aktar.`
- `Yedekten VS Code ayarlarımı ve eklenti listemi geri yükle.`
- `.ssh anahtarlarımı ve .gitconfig dosyamı yedekten kopyala.`

English:

- `Move my Chrome settings and bookmarks from my backup into this system.`
- `Restore my Desktop and Documents from the backup on G:, but don't overwrite anything.`
- `Copy my Firefox profile from the old Windows in my backup image.`
- `Bring back my Windows Terminal settings and my SSH keys from the backup.`

Claude answers in the language you write in.

## What transfers and what does not

| Transfers | Does not transfer after a reset |
| --- | --- |
| Browser bookmarks, history, open tabs/sessions, extensions, settings, search engines, autofill addresses | **Chromium browser passwords, cookies (site logins) and payment cards** (Chrome, Edge, Brave, Opera, Vivaldi) |
| **Firefox: everything**, including passwords and cookies (you need your Primary Password if you set one) | Wi-Fi passwords |
| Your files: Desktop, Documents, Downloads, Pictures, Music, Videos | Windows Credential Manager entries (network, RDP, Git Credential Manager) |
| VS Code settings, keybindings, snippets; extensions reinstalled from the list | Sign-ins of apps such as Teams, Slack, Discord, Spotify, VS Code accounts |
| Windows Terminal settings, `.ssh` keys, `.gitconfig`, Notepad++ settings and unsaved tabs | Windows Hello, EFS-encrypted files, certificates' private keys |
| | Installed programs: reinstall them |

**Why Chromium passwords are lost.** Chrome, Edge and other Chromium browsers encrypt passwords, cookies and cards with a key in their `Local State` file. That key is protected by your old Windows account's DPAPI keys, and since Chrome 127 also by the old machine's system key ("app-bound encryption"). A reset creates a new account with new keys, even with the same user name and password, so the old key can no longer be unlocked. Copying the profile brings back everything else. The skill keeps the new browser's `Local State` and does not copy the undecryptable password and cookie databases; they remain in your image.

## Before you reset Windows: do this first

The skill cannot recover browser passwords after a reset, so secure them **before** resetting:

- **Turn on browser sync**, and make sure *Passwords* is included: Chrome → Settings → You and Google → Sync; Edge → Settings → Profiles → Sync. After the reset, sign in again and the passwords come back.
- **Or export your passwords to CSV**: Chrome `chrome://password-manager/settings` → *Export passwords*; Edge `edge://wallet/passwords` → *Export passwords*. Store the file somewhere safe, import it after the reset on the same page, and then delete it. The file is unencrypted.
- Export Wi-Fi profiles if you need them: run `netsh wlan export profile key=clear folder=<folder>` from an elevated prompt. Keep that folder private.
- Note the accounts you will need to sign in to again (Microsoft, Google, GitHub, chat apps), and make sure you know those passwords or have their two-factor methods.
- Take a fresh inocrium backup right before the reset, and verify it.

## Limits

- The skill copies data. It does not reinstall programs or restore registry settings.
- It works one app at a time and asks before replacing anything. Renamed `*.before-restore-*` folders stay until you delete them.
- A mounted image is read through the inocrium service. Closing Task Manager and any Explorer windows on the mounted drive helps the final unmount succeed.
- The skill depends on the inocrium mount path, which is still early-stage. If something fails, the inocrium GUI's **Mounts** page shows and removes mounts.
