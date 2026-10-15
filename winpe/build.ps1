<#
.SYNOPSIS
Builds the inocrium reflect rescue media: a small Windows PE with the CLI and a restore menu.

.DESCRIPTION
Run elevated, with the Windows ADK deployment tools and the Windows PE add-on installed. Produces
out\media (the files for a USB drive; see New-InocriumPeUsb.ps1) and out\inocrium-pe-amd64.iso
(for Rufus, Ventoy, a DVD or a VM). The CLI must be built with the static C runtime, as the
workspace's .cargo\config.toml does, because Windows PE has no VC++ runtime.

.PARAMETER BootEx
Use the boot manager signed with the "Windows UEFI CA 2023" certificate. Needed on PCs whose
Secure Boot no longer trusts the 2011 certificate; such PCs must already trust the 2023 one.

.PARAMETER TestMode
Builds an ISO that restores unattended and shuts down, for the build's own Hyper-V test. Never
give such media to a person.
#>
[CmdletBinding()]
param(
    [string] $Cli,
    [string] $OutDir,
    [switch] $BootEx,
    [string] $TestMode
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Cli) { $Cli = Join-Path $here '..\target\main\release\inocrium.exe' }
if (-not $OutDir) { $OutDir = Join-Path $here 'out' }

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this script from an elevated PowerShell.' }
$kits = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots' -ErrorAction SilentlyContinue).KitsRoot10
if (-not $kits) { throw 'The Windows ADK is not installed.' }
$adk = Join-Path $kits 'Assessment and Deployment Kit'
$pe = Join-Path $adk 'Windows Preinstallation Environment\amd64'
$oscdimg = Join-Path $adk 'Deployment Tools\amd64\Oscdimg'
if (-not (Test-Path -LiteralPath (Join-Path $pe 'en-us\winpe.wim'))) { throw 'The Windows PE add-on for the ADK is not installed.' }
$Cli = (Resolve-Path -LiteralPath $Cli).Path

# The CLI must not need the VC++ runtime, which Windows PE lacks.
$bytes = [IO.File]::ReadAllBytes($Cli)
if ([Text.Encoding]::ASCII.GetString($bytes).IndexOf('VCRUNTIME140.dll', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
    throw "$Cli links the VC++ runtime dynamically; build it with the workspace's .cargo\config.toml."
}

# The DLLs a PE32+ executable loads at startup (its import directory; delay-loaded ones excluded).
function Get-StaticImports([string] $Path) {
    $b = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($b, 0x3c)
    $sections = [BitConverter]::ToUInt16($b, $pe + 6)
    $optional = $pe + 24
    $table = $optional + [BitConverter]::ToUInt16($b, $pe + 20)
    $rva = [BitConverter]::ToUInt32($b, $optional + 112 + 8)
    $toOffset = {
        param([uint32] $r)
        for ($i = 0; $i -lt $sections; $i++) {
            $s = $table + 40 * $i
            $va = [BitConverter]::ToUInt32($b, $s + 12)
            $size = [Math]::Max([BitConverter]::ToUInt32($b, $s + 8), [BitConverter]::ToUInt32($b, $s + 16))
            if ($r -ge $va -and $r -lt $va + $size) { return $r - $va + [BitConverter]::ToUInt32($b, $s + 20) }
        }
        throw "RVA $r is outside every section of $Path"
    }
    $names = New-Object 'System.Collections.Generic.List[string]'
    for ($d = & $toOffset $rva; [BitConverter]::ToUInt32($b, $d + 12) -ne 0; $d += 20) {
        $n = & $toOffset ([BitConverter]::ToUInt32($b, $d + 12))
        $end = [Array]::IndexOf($b, [byte]0, [int]$n)
        $names.Add([Text.Encoding]::ASCII.GetString($b, $n, $end - $n).ToLowerInvariant())
    }
    return $names | Sort-Object -Unique
}

# dism.exe rather than the DISM cmdlets, which can take PowerShell down when output is redirected.
function Invoke-Dism {
    $ErrorActionPreference = 'Continue'
    $out = & dism.exe /English @args 2>&1
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) {
        $out | Where-Object { "$_".Trim() } | Select-Object -Last 15 | ForEach-Object { Write-Host "  $_" }
        throw "dism $($args[0]) failed with exit code $LASTEXITCODE"
    }
}

$OutDir = [IO.Path]::GetFullPath($OutDir)
$work = Join-Path $OutDir 'work'
$media = Join-Path $OutDir 'media'
$mount = Join-Path $work 'mount'
# A build that died while the image was mounted leaves the mount behind.
if (Test-Path -LiteralPath (Join-Path $mount 'Windows')) { & dism.exe /English /Unmount-Image "/MountDir:$mount" /Discard | Out-Null }
& dism.exe /English /Cleanup-Mountpoints | Out-Null
foreach ($dir in $work, $media) { if (Test-Path -LiteralPath $dir) { Remove-Item -LiteralPath $dir -Recurse -Force } }
$null = New-Item -ItemType Directory -Path $mount, $media -Force

# copype: the boot files of the media and an untouched Windows PE image.
& robocopy.exe (Join-Path $pe 'Media') $media /E /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "copying the media failed ($LASTEXITCODE)" }
$null = New-Item -ItemType Directory -Path (Join-Path $media 'sources') -Force
$wim = Join-Path $work 'winpe.wim'
Copy-Item -LiteralPath (Join-Path $pe 'en-us\winpe.wim') -Destination $wim
Set-ItemProperty -LiteralPath $wim -Name IsReadOnly -Value $false

Write-Host 'Customising Windows PE...'
Invoke-Dism "/Mount-Image" "/ImageFile:$wim" "/Index:1" "/MountDir:$mount"
$committed = $false
try {
    # PowerShell runs the restore menu; it needs WMI, .NET and scripting support.
    $ocs = Join-Path $pe 'WinPE_OCs'
    foreach ($oc in 'WinPE-WMI', 'WinPE-NetFX', 'WinPE-Scripting', 'WinPE-PowerShell') {
        foreach ($cab in (Join-Path $ocs "$oc.cab"), (Join-Path $ocs "en-us\$($oc)_en-us.cab")) {
            Write-Host "  adding $([IO.Path]::GetFileName($cab))"
            Invoke-Dism "/Image:$mount" "/Add-Package" "/PackagePath:$cab"
        }
    }
    $tool = Join-Path $mount 'inocrium'
    $null = New-Item -ItemType Directory -Path $tool -Force
    Copy-Item -LiteralPath $Cli -Destination (Join-Path $tool 'inocrium.exe')
    Copy-Item -LiteralPath (Join-Path $here 'files\Restore-Menu.ps1') -Destination $tool
    Copy-Item -LiteralPath (Join-Path $here '..\LICENSE') -Destination $tool
    Copy-Item -LiteralPath (Join-Path $here 'files\startnet.cmd') -Destination (Join-Path $mount 'Windows\System32\startnet.cmd') -Force
    if ($TestMode) { Copy-Item -LiteralPath $TestMode -Destination (Join-Path $tool 'autotest.json') }
    # As MakeWinPEMedia does: the media boots the boot manager of this Windows PE build, signed with
    # the 2011 certificate or, with -BootEx, the Windows UEFI CA 2023 one.
    $bootmgr = if ($BootEx) { Join-Path $mount 'Windows\Boot\EFI_EX\bootmgfw_EX.efi' } else { Join-Path $mount 'Windows\Boot\EFI\bootmgfw.efi' }
    foreach ($target in 'EFI\Boot\bootx64.efi', 'EFI\Microsoft\Boot\bootmgfw.efi') {
        Copy-Item -LiteralPath $bootmgr -Destination (Join-Path $media $target) -Force
    }
    $missing = @(Get-StaticImports $Cli | Where-Object { $_ -notmatch '^(api|ext)-ms-' -and -not (Test-Path -LiteralPath (Join-Path $mount "Windows\System32\$_")) })
    if ($missing.Count) { throw "Windows PE lacks $($missing -join ', '), which the CLI imports at startup." }
    # Room for PowerShell and large restores.
    Invoke-Dism "/Image:$mount" "/Set-ScratchSpace:512"
    Invoke-Dism "/Unmount-Image" "/MountDir:$mount" "/Commit"
    $committed = $true
} finally {
    if (-not $committed) { & dism.exe /English /Unmount-Image "/MountDir:$mount" /Discard | Out-Null }
}
Write-Host 'Compressing the image...'
$boot = Join-Path $media 'sources\boot.wim'
Invoke-Dism "/Export-Image" "/SourceImageFile:$wim" "/SourceIndex:1" "/DestinationImageFile:$boot" "/Compress:max"

$suffix = if ($BootEx) { '_EX' } else { '' }
$efisys = if ($TestMode) { "efisys_noprompt$suffix.bin" } else { "efisys$suffix.bin" }
$iso = Join-Path $OutDir $(if ($TestMode) { 'inocrium-pe-amd64-test.iso' } else { 'inocrium-pe-amd64.iso' })
$bootdata = '2#p0,e,b"{0}"#pEF,e,b"{1}"' -f (Join-Path $oscdimg 'etfsboot.com'), (Join-Path $oscdimg $efisys)
$p = Start-Process -FilePath (Join-Path $oscdimg 'oscdimg.exe') -Wait -PassThru -NoNewWindow -ArgumentList @('-m', '-o', '-u2', '-udfver102', "-bootdata:$bootdata", '-lINOCRIUMPE', "`"$media`"", "`"$iso`"")
if ($p.ExitCode -ne 0) { throw "oscdimg failed ($($p.ExitCode))" }
Remove-Item -LiteralPath $work -Recurse -Force
Write-Host ("Done: {0} ({1:N0} MB) and the USB files in {2}" -f $iso, ((Get-Item -LiteralPath $iso).Length / 1MB), $media) -ForegroundColor Green
