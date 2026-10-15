# inocrium reflect rescue media (Windows PE)

A small Windows PE that boots from a USB drive or an ISO and restores a disk or a partition from an
inocrium (`.irimg`) or Macrium Reflect X (`.mrimgx`) image. Use it when Windows does not start, or
to put an image back onto the disk Windows runs from.

## What it contains

- Windows PE from the Windows ADK, with PowerShell (WinPE-WMI, -NetFX, -Scripting, -PowerShell).
- `inocrium.exe` in direct mode, linked with the static C runtime because Windows PE has no VC++
  runtime.
- `Restore-Menu.ps1`, started automatically:
  1. **Restore a whole disk.** Finds images in the root and first-level folders of every drive (or
     takes a typed path) and shows the image's disks and partitions. You then pick the target disk.
     The target must be at least as large as the imaged disk; if it is larger, the last partition can
     grow. The disk and partition GUIDs of the image are kept, so Windows boots as before.
  2. **Restore one partition** over an existing partition of the same size or larger, optionally
     growing the file system.
  3. **Verify an image.** Reads every block and checks it.
  4. **Show disks.**
  5. **Load a storage driver** (`drvload`) for controllers Windows PE does not know, such as
     Intel VMD/RST.
  6. **Command prompt.**
  7. **Restart.**
  8. **Shut down.**

  Every overwrite needs `YES` and then the target disk number. The CLI itself refuses the disk that
  holds the image.

## Build

Requirements:

- an elevated PowerShell;
- the [Windows ADK](https://learn.microsoft.com/windows-hardware/get-started/adk-install) "Deployment Tools" feature;
- the Windows PE add-on;
- a release build of the CLI.

```powershell
cargo build --release --bin inocrium        # from the repository root; static CRT via .cargo\config.toml
.\winpe\build.ps1                           # -> winpe\out\inocrium-pe-amd64.iso and winpe\out\media
```

`-BootEx` uses boot files signed with the *Windows UEFI CA 2023* certificate. It is only needed on
PCs whose Secure Boot has revoked the 2011 certificate, and those PCs already trust the 2023 one.

## Write a USB drive

At least 1 GB; the drive is erased. From an elevated PowerShell:

```powershell
Get-Disk                                     # find the USB drive's number
.\winpe\New-InocriumPeUsb.ps1 -DiskNumber 5
```

The script creates one FAT32 partition, which UEFI firmware boots with Secure Boot on or off. It
refuses the system disk, non-USB disks and disks that hold images. The ISO also works with
[Rufus](https://rufus.ie) or [Ventoy](https://www.ventoy.net).

## Restore after a failed reset

1. Connect the USB disk with the image and the rescue USB drive.
2. Start the PC from the rescue drive with the firmware boot menu (F12 on most Lenovo laptops).
3. Choose **1**, pick the image and the internal disk. Check the model and size shown, then
   confirm.
4. When the restore finishes, remove the rescue drive and restart.

Drive letters in Windows PE differ from those in Windows; identify disks by model and size. A
restore over USB 3 runs at roughly the speed of the slower disk; 230 GB of compressed data takes
about 30–60 minutes.
