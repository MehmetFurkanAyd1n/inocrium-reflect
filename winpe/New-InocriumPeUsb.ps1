<#
.SYNOPSIS
Writes the inocrium reflect rescue media (Windows PE) to a USB drive, erasing the drive.

.DESCRIPTION
Run elevated. The drive gets one FAT32 partition (at most 32 GB) that UEFI firmware boots from,
with Secure Boot on or off. Everything on the drive is erased; the script refuses the system disk,
non-USB disks (unless -AllowNonUsb) and disks that hold backup images.

.EXAMPLE
.\New-InocriumPeUsb.ps1 -DiskNumber 5
.EXAMPLE
.\New-InocriumPeUsb.ps1 -DiskNumber 5 -Media D:\inocrium-pe\media
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][int] $DiskNumber,
    [string] $Media = (Join-Path $PSScriptRoot 'out\media'),
    [switch] $AllowNonUsb
)
$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this script from an elevated PowerShell.' }
foreach ($required in 'EFI\Boot\bootx64.efi', 'EFI\Microsoft\Boot\BCD', 'Boot\boot.sdi', 'sources\boot.wim') {
    if (-not (Test-Path -LiteralPath (Join-Path $Media $required))) { throw "The media folder is incomplete: $required is missing. Build it with build.ps1 first." }
}
$wim = Get-Item -LiteralPath (Join-Path $Media 'sources\boot.wim')
if ($wim.Length -ge 4GB) { throw 'boot.wim is larger than FAT32 allows.' }

$disk = Get-Disk -Number $DiskNumber
if ($disk.IsBoot -or $disk.IsSystem) { throw "Disk $DiskNumber holds the running Windows." }
if ($disk.BusType -ne 'USB' -and -not $AllowNonUsb) { throw "Disk $DiskNumber is not a USB disk ($($disk.BusType)). Pass -AllowNonUsb if this is intended." }
$volumes = @(Get-Partition -DiskNumber $DiskNumber -ErrorAction SilentlyContinue | Where-Object DriveLetter)
foreach ($p in $volumes) {
    $images = @(Get-ChildItem -LiteralPath ($p.DriveLetter + ':\') -Filter '*.irimg' -Recurse -Depth 1 -ErrorAction SilentlyContinue)
    if ($images.Count) { throw "Disk $DiskNumber holds backup images ($($images[0].FullName)); refusing to erase it." }
}

Write-Host ("Disk {0}: {1}, {2:N1} GB, {3}" -f $disk.Number, $disk.FriendlyName, ($disk.Size / 1GB), $disk.BusType)
foreach ($p in @(Get-Partition -DiskNumber $DiskNumber -ErrorAction SilentlyContinue)) {
    $v = $p | Get-Volume -ErrorAction SilentlyContinue
    Write-Host ("  partition {0}  {1}  {2}  {3:N1} GB" -f $p.PartitionNumber, $(if ($p.DriveLetter) { $p.DriveLetter + ':' } else { '  ' }), $v.FileSystemLabel, ($p.Size / 1GB))
}
Write-Host "EVERYTHING ON DISK $DiskNumber WILL BE ERASED." -ForegroundColor Red
if ((Read-Host "Type the disk number ($DiskNumber) to continue") -ne [string]$DiskNumber) { Write-Host 'Cancelled.'; return }

if ($disk.IsOffline) { Set-Disk -Number $DiskNumber -IsOffline $false }
if ($disk.IsReadOnly) { Set-Disk -Number $DiskNumber -IsReadOnly $false }
if ($disk.PartitionStyle -ne 'RAW') { Clear-Disk -Number $DiskNumber -RemoveData -RemoveOEM -Confirm:$false }
$null = Initialize-Disk -Number $DiskNumber -PartitionStyle MBR
$size = [Math]::Min((Get-Disk -Number $DiskNumber).LargestFreeExtent, 32GB)
$partition = New-Partition -DiskNumber $DiskNumber -Size $size -IsActive -AssignDriveLetter
$null = Format-Volume -Partition $partition -FileSystem FAT32 -NewFileSystemLabel 'INOCRIUMPE' -Confirm:$false
$letter = (Get-Partition -DiskNumber $DiskNumber -PartitionNumber $partition.PartitionNumber).DriveLetter
if (-not $letter) { throw 'The new partition got no drive letter.' }
& robocopy.exe $Media ($letter + ':\') /E /R:1 /W:1 /NP /NFL /NDL
if ($LASTEXITCODE -ge 8) { throw "Copying the media failed (robocopy exit $LASTEXITCODE)." }
Write-Host "The rescue USB drive is ready ($letter`:). Boot from it with the firmware boot menu (F12 on most Lenovo laptops)." -ForegroundColor Green
