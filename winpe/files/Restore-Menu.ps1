# inocrium reflect rescue menu for Windows PE: restores a disk or a partition from an image with
# the direct-mode CLI. Started by startnet.cmd; exiting leaves a command prompt.
$ErrorActionPreference = 'Stop'
$cli = Join-Path $PSScriptRoot 'inocrium.exe'
$Host.UI.RawUI.WindowTitle = 'inocrium reflect - Windows PE'

function Format-Size([double] $Bytes) {
    foreach ($unit in 'B', 'KB', 'MB', 'GB', 'TB') {
        if ($Bytes -lt 1024 -or $unit -eq 'TB') { return ('{0:N1} {1}' -f $Bytes, $unit) }
        $Bytes /= 1024
    }
}

# Runs the CLI in direct mode and returns its JSON output. A password prompt for encrypted images
# still reaches the console, because only stdout is captured.
function Invoke-Json([string[]] $Arguments) {
    $out = & $cli --direct --json @Arguments
    if ($LASTEXITCODE -ne 0) { throw "inocrium $($Arguments[0]) failed (exit $LASTEXITCODE)" }
    $text = ($out | Where-Object { $_ -match '^\s*[\[{]' } | Select-Object -Last 1)
    if (-not $text) { return $null }
    return ($text | ConvertFrom-Json)
}

function Read-Choice([string] $Prompt, [int] $Count) {
    while ($true) {
        $value = Read-Host $Prompt
        if ($value -eq '') { return $null }
        $n = 0
        if ([int]::TryParse($value, [ref]$n) -and $n -ge 1 -and $n -le $Count) { return $n - 1 }
        Write-Host "Enter a number from 1 to $Count, or press Enter to go back." -ForegroundColor Yellow
    }
}

function Find-Images {
    $found = New-Object 'System.Collections.Generic.List[object]'
    foreach ($drive in [IO.DriveInfo]::GetDrives()) {
        if (-not $drive.IsReady -or $drive.Name -like 'X:*') { continue }
        $dirs = New-Object 'System.Collections.Generic.List[string]'
        $dirs.Add($drive.RootDirectory.FullName)
        try { foreach ($d in [IO.Directory]::GetDirectories($drive.RootDirectory.FullName)) { $dirs.Add($d) } } catch { }
        foreach ($dir in $dirs) {
            try {
                foreach ($pattern in '*.irimg', '*.mrimgx') {
                    foreach ($file in [IO.Directory]::GetFiles($dir, $pattern)) { $found.Add((Get-Item -LiteralPath $file)) }
                }
            } catch { }
        }
    }
    return $found
}

function Select-Image {
    Write-Host "`nSearching the drives for images..."
    $images = @(Find-Images | Sort-Object LastWriteTime -Descending)
    for ($i = 0; $i -lt $images.Count; $i++) {
        Write-Host ('  {0}) {1}  {2}  {3}' -f ($i + 1), $images[$i].FullName, (Format-Size $images[$i].Length), $images[$i].LastWriteTime.ToString('yyyy-MM-dd HH:mm'))
    }
    if ($images.Count -eq 0) { Write-Host '  No images found in the root or first-level folders of any drive.' }
    Write-Host '  P) Type a path'
    $value = Read-Host 'Image'
    if ($value -eq '') { return $null }
    if ($value -match '^[Pp]$') {
        $path = (Read-Host 'Full path of the .irimg or .mrimgx file').Trim('"', ' ')
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Write-Host 'File not found.' -ForegroundColor Yellow; return $null }
        return $path
    }
    $n = 0
    if ([int]::TryParse($value, [ref]$n) -and $n -ge 1 -and $n -le $images.Count) { return $images[$n - 1].FullName }
    return $null
}

function Show-Image($Info) {
    Write-Host ("`nImage created {0}, {1} on disk{2}" -f $Info.created, (Format-Size $Info.sizeOnDisk), $(if ($Info.encrypted) { ', encrypted' } else { '' }))
    if ($Info.incomplete) { Write-Host 'WARNING: this backup did not finish; it cannot be restored.' -ForegroundColor Red }
    foreach ($d in $Info.disks) {
        Write-Host ("  Image disk {0}: {1} {2}" -f $d.index, $d.style, (Format-Size $d.size))
        foreach ($p in $d.partitions) {
            Write-Host ("    partition {0}  {1,-6} {2,-12} {3}" -f $p.number, $(if ($p.fileSystem) { $p.fileSystem } else { '-' }), $(if ($p.label) { $p.label } else { '' }), (Format-Size $p.length))
        }
    }
}

function Show-Disks($Disks) {
    for ($i = 0; $i -lt $Disks.Count; $i++) {
        $d = $Disks[$i]
        Write-Host ('  {0}) Disk {1}  {2}  {3}  {4}' -f ($i + 1), $d.number, $d.model, (Format-Size $d.size), $d.style)
        foreach ($p in $d.partitions) {
            $letter = if ($p.letter) { $p.letter + ':' } else { '  ' }
            Write-Host ("       partition {0}  {1} {2,-6} {3,-14} {4}" -f $p.number, $letter, $(if ($p.fileSystem) { $p.fileSystem } else { '-' }), $(if ($p.label) { $p.label } else { '' }), (Format-Size $p.length))
        }
    }
}

function Get-Disks { @(Invoke-Json @('disks')) }

function Confirm-Overwrite([string] $What, [int] $DiskNumber) {
    Write-Host "`n$What" -ForegroundColor Red
    Write-Host 'Everything that is there now will be replaced. This cannot be undone.' -ForegroundColor Red
    $answer = Read-Host "Type YES to continue"
    if ($answer -cne 'YES') { Write-Host 'Cancelled.'; return $false }
    $again = Read-Host "Type the target disk number ($DiskNumber) once more"
    if ($again -ne [string]$DiskNumber) { Write-Host 'Cancelled.'; return $false }
    return $true
}

function Invoke-Restore([string[]] $Arguments) {
    $started = Get-Date
    & $cli --direct restore @Arguments --yes
    $code = $LASTEXITCODE
    $took = (Get-Date) - $started
    if ($code -eq 0) {
        Write-Host ("`nRestore finished in {0:hh\:mm\:ss}." -f $took) -ForegroundColor Green
        Write-Host 'Remove the rescue USB stick before restarting, or choose the internal disk in the boot menu.'
    } else {
        Write-Host "`nRestore FAILED (exit $code). The target disk may now be incomplete; run the restore again." -ForegroundColor Red
    }
}

function Restore-Disk {
    $image = Select-Image; if (-not $image) { return }
    $info = Invoke-Json @('info', $image)
    Show-Image $info
    if ($info.incomplete) { return }
    $imageDisk = 0
    if (@($info.disks).Count -gt 1) {
        $c = Read-Choice 'Image disk to restore (1 = image disk 0)' @($info.disks).Count
        if ($null -eq $c) { return }
        $imageDisk = [int]$info.disks[$c].index
    }
    $source = $info.disks | Where-Object { $_.index -eq $imageDisk }
    $disks = Get-Disks
    Write-Host "`nTarget disks:"
    Show-Disks $disks
    $t = Read-Choice 'Target disk' $disks.Count
    if ($null -eq $t) { return }
    $target = $disks[$t]
    if ($target.size -lt $source.size) {
        Write-Host ("Disk {0} ({1}) is smaller than the image disk ({2}); restore needs a disk at least as large." -f $target.number, (Format-Size $target.size), (Format-Size $source.size)) -ForegroundColor Yellow
        return
    }
    $arguments = @($image, '--disk', [string]$imageDisk, '--target-disk', [string]$target.number)
    if ($target.size -gt $source.size) {
        if ((Read-Host 'The target is larger. Grow the last partition to fill it? (y/N)') -match '^[Yy]') { $arguments += '--extend' }
    }
    if (-not (Confirm-Overwrite ("Image disk {0} ({1}) will be written to disk {2}: {3}, {4}." -f $imageDisk, (Format-Size $source.size), $target.number, $target.model, (Format-Size $target.size)) $target.number)) { return }
    Invoke-Restore $arguments
}

function Restore-Partition {
    $image = Select-Image; if (-not $image) { return }
    $info = Invoke-Json @('info', $image)
    Show-Image $info
    if ($info.incomplete) { return }
    $parts = @(foreach ($d in $info.disks) { foreach ($p in $d.partitions) { if ($p.captured) { [pscustomobject]@{ Disk = $d.index; Part = $p } } } })
    for ($i = 0; $i -lt $parts.Count; $i++) {
        $p = $parts[$i].Part
        Write-Host ('  {0}) image disk {1} partition {2}  {3}  {4}' -f ($i + 1), $parts[$i].Disk, $p.number, $(if ($p.fileSystem) { $p.fileSystem } else { '-' }), (Format-Size $p.length))
    }
    $c = Read-Choice 'Partition to restore' $parts.Count
    if ($null -eq $c) { return }
    $source = $parts[$c]
    $disks = Get-Disks
    Write-Host "`nTarget disks:"
    Show-Disks $disks
    $t = Read-Choice 'Target disk' $disks.Count
    if ($null -eq $t) { return }
    $target = $disks[$t]
    $existing = @($target.partitions)
    for ($i = 0; $i -lt $existing.Count; $i++) {
        Write-Host ('  {0}) replace partition {1}  {2}  {3}' -f ($i + 1), $existing[$i].number, $(if ($existing[$i].fileSystem) { $existing[$i].fileSystem } else { '-' }), (Format-Size $existing[$i].length))
    }
    $r = Read-Choice 'Partition to replace' $existing.Count
    if ($null -eq $r) { return }
    $replace = $existing[$r]
    if ($replace.length -lt $source.Part.length) {
        Write-Host ("Partition {0} ({1}) is smaller than the image partition ({2})." -f $replace.number, (Format-Size $replace.length), (Format-Size $source.Part.length)) -ForegroundColor Yellow
        return
    }
    $arguments = @($image, '--disk', [string]$source.Disk, '--partition', [string]$source.Part.number,
        '--target-disk', [string]$target.number, '--target-partition', [string]$replace.number)
    if ($replace.length -gt $source.Part.length) {
        if ((Read-Host 'The target partition is larger. Grow the file system to fill it? (y/N)') -match '^[Yy]') { $arguments += '--extend' }
    }
    if (-not (Confirm-Overwrite ("Image partition {0} will be written over partition {1} of disk {2} ({3})." -f $source.Part.number, $replace.number, $target.number, $target.model) $target.number)) { return }
    Invoke-Restore $arguments
}

function Test-Image {
    $image = Select-Image; if (-not $image) { return }
    & $cli --direct verify $image
    if ($LASTEXITCODE -eq 0) { Write-Host 'The image is intact.' -ForegroundColor Green } else { Write-Host "Verification FAILED (exit $LASTEXITCODE)." -ForegroundColor Red }
}

function Add-Driver {
    $path = (Read-Host 'Path of the driver .inf file (for example E:\drivers\iaStorVD.inf)').Trim('"', ' ')
    if ($path) { & drvload.exe $path }
}

# Unattended restore for the build's own Hyper-V test; only test builds contain autotest.json.
function Invoke-AutoTest([string] $Config) {
    # The CLI reports progress on stderr, which must not stop the script.
    $ErrorActionPreference = 'Continue'
    $test = Get-Content -LiteralPath $Config -Raw | ConvertFrom-Json
    $image = @(Find-Images | Where-Object { $_.Name -eq $test.image })[0].FullName
    $resultPath = Join-Path ([IO.Path]::GetPathRoot($image)) 'pe-autotest-result.txt'
    $lines = New-Object 'System.Collections.Generic.List[string]'
    try {
        $lines.Add("image $image")
        $disks = Get-Disks
        foreach ($d in $disks) { $lines.Add("disk $($d.number) size $($d.size) style $($d.style) partitions $(@($d.partitions).Count) model $($d.model)") }
        $target = @($disks | Where-Object { [long]$_.size -eq [long]$test.targetSize -and @($_.partitions).Count -eq 0 })
        if ($target.Count -ne 1) { throw "expected one blank $($test.targetSize)-byte disk, found $($target.Count)" }
        $lines.Add("target disk $($target[0].number)")
        $out = & $cli --direct restore $image --disk 0 --target-disk ([string]$target[0].number) --yes 2>&1
        $lines.Add("restore exit $LASTEXITCODE")
        foreach ($l in $out) { $lines.Add([string]$l) }
    } catch { $lines.Add("error: $($_.Exception.Message)") }
    [IO.File]::WriteAllLines($resultPath, $lines)
    & wpeutil.exe shutdown
}

$autotest = Join-Path $PSScriptRoot 'autotest.json'
if (Test-Path -LiteralPath $autotest) { Invoke-AutoTest $autotest; return }

while ($true) {
    Write-Host ''
    Write-Host '==================== inocrium reflect rescue ====================' -ForegroundColor Cyan
    Write-Host '  1) Restore a whole disk from an image'
    Write-Host '  2) Restore one partition from an image'
    Write-Host '  3) Verify an image'
    Write-Host '  4) Show disks'
    Write-Host '  5) Load a storage driver (.inf)'
    Write-Host '  6) Command prompt'
    Write-Host '  7) Restart'
    Write-Host '  8) Shut down'
    $choice = Read-Host 'Choice'
    try {
        switch ($choice) {
            '1' { Restore-Disk }
            '2' { Restore-Partition }
            '3' { Test-Image }
            '4' { Show-Disks (Get-Disks) }
            '5' { Add-Driver }
            '6' { Start-Process cmd.exe -Wait; }
            '7' { & wpeutil.exe reboot }
            '8' { & wpeutil.exe shutdown }
        }
    } catch {
        Write-Host "Error: $($_.Exception.Message)" -ForegroundColor Red
    }
}
