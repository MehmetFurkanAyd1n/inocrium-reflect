#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Test-Administrator {
    [CmdletBinding()]
    param()
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function New-TestWorkspace {
    [CmdletBinding()]
    param()
    if (-not (Test-Administrator)) { throw 'These fixtures require an elevated PowerShell.' }
    $root = Join-Path $env:TEMP 'inocrium-tests'
    $path = Join-Path $root ([guid]::NewGuid().ToString('D'))
    $null = New-Item -ItemType Directory -Path $path -Force
    [pscustomobject]@{
        Path = [IO.Path]::GetFullPath($path)
        Vhds = New-Object 'System.Collections.Generic.List[object]'
        Mounts = New-Object 'System.Collections.Generic.List[object]'
        Processes = New-Object 'System.Collections.Generic.List[object]'
        CleanupBlockers = New-Object 'System.Collections.Generic.List[string]'
    }
}

function Test-WorkspacePath {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Workspace, [Parameter(Mandatory)][string] $Path)
    $root = [IO.Path]::GetFullPath((Join-Path $env:TEMP 'inocrium-tests')).TrimEnd('\') + '\'
    $workspacePath = [IO.Path]::GetFullPath($Workspace.Path).TrimEnd('\')
    $leaf = Split-Path -Leaf $workspacePath
    if (-not $workspacePath.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path -Parent $workspacePath).TrimEnd('\') -ne $root.TrimEnd('\') -or
        $leaf -notmatch '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$') {
        throw "Unsafe test workspace: $workspacePath"
    }
    $fullPath = [IO.Path]::GetFullPath($Path)
    if ($fullPath -ne $workspacePath -and
        -not $fullPath.StartsWith($workspacePath + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Path is outside the test workspace: $fullPath"
    }
    if ($fullPath -match '["\r\n]') { throw 'Quotes or newlines are forbidden in generated test paths.' }
    # Do not follow junctions/symlinks during cleanup or disk provisioning.
    $cursor = $fullPath
    while ($cursor.Length -ge $root.TrimEnd('\').Length) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse point in test path: $cursor"
            }
        }
        $parent = Split-Path -Parent $cursor
        if ([string]::IsNullOrEmpty($parent) -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
    return $fullPath
}

function Invoke-TestDiskPart {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Workspace, [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][ValidateSet('Create', 'Attach', 'Detach')][string] $Operation,
        [long] $SizeBytes = 1GB, [switch] $ReadOnly)
    $safePath = Test-WorkspacePath -Workspace $Workspace -Path $Path
    if ([IO.Path]::GetExtension($safePath) -ne '.vhdx') { throw 'Only generated .vhdx files are allowed.' }
    $lines = @('select vdisk file="{0}"' -f $safePath)
    switch ($Operation) {
        'Create' {
            $megabytes = [long][Math]::Ceiling($SizeBytes / 1MB)
            if ($megabytes -lt 128 -or $megabytes -gt 4096) { throw 'Test disks must be between 128 MiB and 4 GiB.' }
            $lines = @('create vdisk file="{0}" maximum={1} type=expandable' -f $safePath, $megabytes)
        }
        'Attach' { $lines += $(if ($ReadOnly) { 'attach vdisk readonly' } else { 'attach vdisk' }) }
        'Detach' { $lines += 'detach vdisk' }
    }
    $scriptPath = Join-Path $Workspace.Path ('diskpart-{0}.txt' -f [guid]::NewGuid().ToString('N'))
    try {
        [IO.File]::WriteAllLines($scriptPath, [string[]]($lines + 'exit'), [Text.Encoding]::Unicode)
        $output = & "$env:SystemRoot\System32\diskpart.exe" /s $scriptPath 2>&1
        if ($LASTEXITCODE -ne 0) { throw "diskpart $Operation failed ($LASTEXITCODE): $($output -join [Environment]::NewLine)" }
        # diskpart sometimes returns zero on command failure; callers verify file/attachment state.
    }
    finally { if (Test-Path -LiteralPath $scriptPath) { Remove-Item -LiteralPath $scriptPath -Force } }
}

function New-TestVhd {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Workspace, [long] $SizeBytes = 1GB,
        [string] $ExistingPath, [switch] $ReadOnly)
    if (-not (Test-Administrator)) { throw 'VHD provisioning requires Administrator.' }
    if ($SizeBytes -lt 128MB -or $SizeBytes -gt 4GB) { throw 'Test disks must be between 128 MiB and 4 GiB.' }
    $existing = -not [string]::IsNullOrWhiteSpace($ExistingPath)
    $path = if ($existing) { $ExistingPath } else { Join-Path $Workspace.Path ('disk-{0}.vhdx' -f [guid]::NewGuid().ToString('N')) }
    $path = Test-WorkspacePath -Workspace $Workspace -Path $path
    if ([IO.Path]::GetExtension($path) -ne '.vhdx') { throw 'Expected a .vhdx path.' }
    if ($existing -and -not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Exported VHDX does not exist: $path" }
    if (-not $existing -and (Test-Path -LiteralPath $path)) { throw 'Refusing to overwrite a VHDX.' }
    $hyperV = @(Get-Command New-VHD, Mount-VHD, Dismount-VHD -ErrorAction SilentlyContinue).Count -eq 3
    $vhd = [pscustomobject]@{
        Path = $path; Backend = $(if ($hyperV) { 'HyperV' } else { 'DiskPart' })
        Workspace = $Workspace; Attached = $false; DiskNumber = $null; ReadOnly = [bool]$ReadOnly
    }
    # Register before creating/attaching, so a partial failure is still cleaned up.
    $Workspace.Vhds.Add($vhd)
    if (-not $existing) {
        if ($hyperV) { $null = New-VHD -Path $path -SizeBytes $SizeBytes -Dynamic }
        else { Invoke-TestDiskPart -Workspace $Workspace -Path $path -Operation Create -SizeBytes $SizeBytes }
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'VHDX creation did not produce a file.' }
    }
    $vhd.Attached = $true
    if ($hyperV) { $null = Mount-VHD -Path $path -ReadOnly:$ReadOnly -Passthru }
    else { Invoke-TestDiskPart -Workspace $Workspace -Path $path -Operation Attach -ReadOnly:$ReadOnly }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $image = Get-DiskImage -ImagePath $path -ErrorAction SilentlyContinue
        if ($null -ne $image -and $image.Attached) {
            $disks = @($image | Get-Disk -ErrorAction SilentlyContinue)
            if ($disks.Count -eq 1) { $vhd.DiskNumber = [int]$disks[0].Number; break }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($null -eq $vhd.DiskNumber) { throw "Attached VHDX could not be resolved to exactly one disk: $path" }
    $null = Get-TestDisk -Vhd $vhd
    return $vhd
}

function Get-TestDisk {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Vhd)
    $null = Test-WorkspacePath -Workspace $Vhd.Workspace -Path $Vhd.Path
    $image = Get-DiskImage -ImagePath $Vhd.Path
    if (-not $image.Attached) { throw 'The test VHDX is no longer attached.' }
    $disks = @($image | Get-Disk)
    if ($disks.Count -ne 1) { throw 'VHDX must map to exactly one disk.' }
    $disk = $disks[0]
    if ($disk.IsBoot -or $disk.IsSystem -or $disk.Number -ne $Vhd.DiskNumber) {
        throw 'Refusing to operate on a system disk or a disk whose number changed.'
    }
    return $disk
}

function Initialize-TestDisk {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Vhd, [ValidateSet('GPT', 'MBR')][string] $PartitionStyle = 'GPT',
        [switch] $IncludeFat32)
    $disk = Get-TestDisk -Vhd $Vhd
    if ($disk.PartitionStyle -ne 'RAW' -or $Vhd.ReadOnly) { throw 'Only a new writable RAW test VHDX may be initialized.' }
    Set-Disk -Number $disk.Number -IsOffline $false
    Set-Disk -Number $disk.Number -IsReadOnly $false
    $null = Initialize-Disk -Number $disk.Number -PartitionStyle $PartitionStyle
    $parts = New-Object 'System.Collections.Generic.List[object]'
    if ($IncludeFat32) {
        $fat = New-Partition -DiskNumber $disk.Number -Size 64MB -AssignDriveLetter
        $null = Format-Volume -Partition $fat -FileSystem FAT32 -NewFileSystemLabel 'IR_FAT' -Confirm:$false
        $parts.Add([pscustomobject]@{ Number = $fat.PartitionNumber; Offset = $fat.Offset; Size = $fat.Size; FileSystem = 'FAT32'; Letter = [string]$fat.DriveLetter })
    }
    # NTFS is last so --extend can grow it while leaving FAT32 intact.
    $ntfs = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    $null = Format-Volume -Partition $ntfs -FileSystem NTFS -NewFileSystemLabel 'IR_NTFS' -Confirm:$false
    $parts.Add([pscustomobject]@{ Number = $ntfs.PartitionNumber; Offset = $ntfs.Offset; Size = $ntfs.Size; FileSystem = 'NTFS'; Letter = [string]$ntfs.DriveLetter })
    return $parts.ToArray()
}

function Write-RandomFiles {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Root, [long[]] $Sizes = @(4096, 1MB, 50MB, 32MB))
    $null = New-Item -ItemType Directory -Path (Join-Path $Root 'nested dir') -Force
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $buffer = New-Object byte[] (1MB)
    try {
        for ($index = 0; $index -lt $Sizes.Count; $index++) {
            if ($Sizes[$index] -lt 0) { throw 'Negative file sizes are invalid.' }
            $relative = if ($index % 2 -eq 0) { 'random-{0}.bin' -f $index } else { 'nested dir\random {0}.bin' -f $index }
            $stream = [IO.File]::Open((Join-Path $Root $relative), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try {
                $remaining = $Sizes[$index]
                while ($remaining -gt 0) {
                    $rng.GetBytes($buffer)
                    $count = [int][Math]::Min($remaining, $buffer.Length)
                    $stream.Write($buffer, 0, $count)
                    $remaining -= $count
                }
                $stream.Flush($true)
            }
            finally { $stream.Dispose() }
        }
    }
    finally { $rng.Dispose() }
    return Get-FileHashes -Root $Root
}

function Get-FileHashes {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Root)
    $fullRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\') + '\'
    if (-not (Test-Path -LiteralPath $fullRoot -PathType Container)) { throw "Payload directory missing: $fullRoot" }
    $hashes = @{}
    foreach ($file in Get-ChildItem -LiteralPath $fullRoot -File -Recurse -Force) {
        $relative = $file.FullName.Substring($fullRoot.Length)
        $hashes[$relative] = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
    }
    return $hashes
}

function Compare-FileHashes {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable] $Expected, [Parameter(Mandatory)][hashtable] $Actual)
    if ($Expected.Count -ne $Actual.Count) { throw "Payload file counts differ: expected $($Expected.Count), got $($Actual.Count)." }
    foreach ($name in $Expected.Keys) {
        if (-not $Actual.ContainsKey($name) -or $Expected[$name] -ne $Actual[$name]) { throw "SHA256 mismatch or missing file: $name" }
    }
    return $true
}

function Get-QuotedNativeArgument {
    [CmdletBinding()]
    param([AllowEmptyString()][Parameter(Mandatory)][string] $Value)
    # Windows CommandLineToArgvW/CRT quoting, including quotes and trailing backslashes.
    $builder = New-Object Text.StringBuilder
    $null = $builder.Append('"')
    $slashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') { $slashes++; continue }
        if ($character -eq '"') {
            $null = $builder.Append(('\' * (2 * $slashes + 1)))
        }
        elseif ($slashes -gt 0) { $null = $builder.Append(('\' * $slashes)) }
        $null = $builder.Append($character)
        $slashes = 0
    }
    if ($slashes -gt 0) { $null = $builder.Append(('\' * (2 * $slashes))) }
    $null = $builder.Append('"')
    return $builder.ToString()
}

function Invoke-Inocrium {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Exe, [Parameter(Mandatory)][AllowEmptyString()][string[]] $Arguments,
        [switch] $Service, [AllowEmptyString()][string] $Password,
        [ValidateRange(1, 2147483)][int] $TimeoutSeconds = 600, [switch] $Background, $Workspace,
        [switch] $ConsoleHost)
    if ($Background -and $null -eq $Workspace) { throw 'Background invocations must be registered to a test workspace.' }
    if (@($Arguments | Where-Object { $_ -match '^--password(?:=|$)' }).Count -gt 0) {
        throw 'Passwords must never be passed as arguments.'
    }
    $nativeArguments = @('--json')
    if (-not $Service) { $nativeArguments += '--direct' }
    $nativeArguments += $Arguments
    $stopPath = $null
    if ($Background -and -not $ConsoleHost) {
        # A private console lets the host deliver Ctrl+C without signalling this
        # PowerShell or any unrelated CLI. TerminateProcess bypasses mount teardown.
        $configPath = Test-WorkspacePath -Workspace $Workspace -Path (Join-Path $Workspace.Path ('mount-host-{0}.json' -f [guid]::NewGuid().ToString('N')))
        $stopPath = $configPath + '.stop'
        $config = @{ Exe = $Exe; Arguments = $Arguments; StopPath = $stopPath }
        if ($PSBoundParameters.ContainsKey('Password')) { $config.Password = $Password }
        [IO.File]::WriteAllText($configPath, ($config | ConvertTo-Json -Depth 4), (New-Object Text.UTF8Encoding($false)))
        $Exe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $nativeArguments = @('-NoProfile', '-NonInteractive', '-File', (Join-Path $PSScriptRoot 'Direct-MountHost.ps1'), '-ConfigPath', $configPath)
    }
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Exe
    $startInfo.Arguments = ($nativeArguments | ForEach-Object { Get-QuotedNativeArgument -Value $_ }) -join ' '
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = -not $ConsoleHost
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.StandardOutputEncoding = New-Object Text.UTF8Encoding($false)
    $startInfo.StandardErrorEncoding = New-Object Text.UTF8Encoding($false)
    # Do not inherit a user's real password. Only this child receives the synthetic password.
    $startInfo.EnvironmentVariables.Remove('INOCRIUM_PASSWORD')
    if ($PSBoundParameters.ContainsKey('Password')) { $startInfo.EnvironmentVariables['INOCRIUM_PASSWORD'] = $Password }
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    $handedOff = $false
    try {
        $null = $process.Start()
        $handle = [pscustomobject]@{
            Process = $process; StdoutTask = $process.StandardOutput.ReadToEndAsync()
            StderrTask = $process.StandardError.ReadToEndAsync(); Closed = $false; StopPath = $stopPath
        }
        if ($Background) { $Workspace.Processes.Add($handle); $handedOff = $true }
        $process.StandardInput.Close()
        if ($Background) { return $handle }
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "CLI timed out after $TimeoutSeconds seconds: $($Arguments[0])"
        }
        [pscustomobject]@{ ExitCode = $process.ExitCode; Stdout = $handle.StdoutTask.Result; Stderr = $handle.StderrTask.Result }
    }
    finally { if (-not $handedOff) { $process.Dispose() } }
}

function ConvertFrom-InocriumJson {
    [CmdletBinding()]
    param([AllowEmptyString()][string] $Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return $null }
    try { return ,(ConvertFrom-Json -InputObject $Text -ErrorAction Stop) } catch { }
    # Also accept newline-delimited JSON mixed with progress lines, last object first.
    $lines = @($Text -split '\r?\n')
    for ($index = $lines.Count - 1; $index -ge 0; $index--) {
        $line = $lines[$index].Trim()
        if (-not ($line.StartsWith('{') -or $line.StartsWith('['))) { continue }
        try { return ,(ConvertFrom-Json -InputObject $line -ErrorAction Stop) } catch { }
    }
    return $null
}

function Get-InocriumJsonValues {
    [CmdletBinding()]
    param($Object, [Parameter(Mandatory)][string[]] $Names)
    if ($null -eq $Object -or $Object -is [string] -or $Object -is [ValueType]) { return }
    if ($Object -is [System.Collections.IDictionary]) {
        foreach ($key in $Object.Keys) {
            if ([string]$key -in $Names) { $Object[$key] }
            Get-InocriumJsonValues -Object $Object[$key] -Names $Names
        }
    }
    elseif ($Object -is [System.Collections.IEnumerable]) {
        foreach ($item in $Object) { Get-InocriumJsonValues -Object $item -Names $Names }
    }
    else {
        foreach ($property in $Object.PSObject.Properties) {
            if ($property.Name -in $Names) { $property.Value }
            Get-InocriumJsonValues -Object $property.Value -Names $Names
        }
    }
}

function Get-NewDriveLetter {
    [CmdletBinding()]
    param([string[]] $Before = @(), $Json, [Parameter(Mandatory)][string] $PayloadName,
        [int] $TimeoutSeconds = 30)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $candidates = @()
        foreach ($value in @(Get-InocriumJsonValues -Object $Json -Names @('letters', 'drive_letter', 'driveLetter', 'mount_points', 'mountPoints'))) {
            if ([string]$value -match '^([A-Za-z]):?[\\/]?$') { $candidates += $Matches[1].ToUpperInvariant() }
        }
        $candidates += @(Get-Volume | Where-Object { $_.DriveLetter -and [string]$_.DriveLetter -notin $Before } |
            ForEach-Object { [string]$_.DriveLetter })
        foreach ($letter in @($candidates | Select-Object -Unique)) {
            if ([IO.Directory]::Exists([IO.Path]::Combine($letter + ':\', $PayloadName))) { return $letter }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    return $null
}

function Get-TestPartitionRoot {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Vhd, [Parameter(Mandatory)][int] $PartitionNumber)
    $disk = Get-TestDisk -Vhd $Vhd
    if ($disk.IsOffline -and -not $Vhd.ReadOnly) { Set-Disk -Number $disk.Number -IsOffline $false }
    Update-HostStorageCache
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $PartitionNumber -ErrorAction SilentlyContinue
        if ($null -ne $partition) {
            if (-not $partition.DriveLetter) {
                $null = Add-PartitionAccessPath -DiskNumber $disk.Number -PartitionNumber $PartitionNumber -AssignDriveLetter
                $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $PartitionNumber
            }
            if ($partition.DriveLetter -and (Test-Path -LiteralPath ($partition.DriveLetter + ':\'))) {
                return ($partition.DriveLetter + ':\')
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Restored partition $PartitionNumber did not appear."
}

function Remove-TestVhd {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Vhd)
    $null = Test-WorkspacePath -Workspace $Vhd.Workspace -Path $Vhd.Path
    if (-not (Test-Path -LiteralPath $Vhd.Path)) { $Vhd.Attached = $false; return }
    $image = Get-DiskImage -ImagePath $Vhd.Path -ErrorAction SilentlyContinue
    if ($null -ne $image -and -not $image.Attached) { $Vhd.Attached = $false; return }
    if (-not $Vhd.Attached -and $null -eq $image) { return }
    if ($Vhd.Backend -eq 'HyperV') { Dismount-VHD -Path $Vhd.Path }
    else { Invoke-TestDiskPart -Workspace $Vhd.Workspace -Path $Vhd.Path -Operation Detach }
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $image = Get-DiskImage -ImagePath $Vhd.Path -ErrorAction SilentlyContinue
        if ($null -ne $image -and -not $image.Attached) { $Vhd.Attached = $false; return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Could not confirm VHDX detachment; workspace preserved: $($Vhd.Path)"
}

function Stop-InocriumProcess {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Handle)
    if ($Handle.Closed) { return }
    try {
        if (-not $Handle.Process.HasExited) {
            if (-not $Handle.StopPath) { throw 'Background process has no private console stop channel; refusing to kill it.' }
            [IO.File]::WriteAllText($Handle.StopPath, 'stop')
        }
        if (-not $Handle.Process.WaitForExit(30000)) { throw 'Background mount did not stop gracefully; process and workspace preserved.' }
        # Drain both pipes before disposing the process; useful diagnostics on mount failure.
        $null = $Handle.StdoutTask.Result
        $null = $Handle.StderrTask.Result
        $Handle.Closed = $true
    }
    finally { if ($Handle.Closed) { $Handle.Process.Dispose() } }
}

function Start-TestImageMount {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Workspace, [Parameter(Mandatory)][string] $Exe,
        [Parameter(Mandatory)][string] $Image, [Parameter(Mandatory)][string] $PayloadName,
        [switch] $Writable)
    $before = @(Get-Volume | Where-Object { $_.DriveLetter } | ForEach-Object { [string]$_.DriveLetter })
    $arguments = @('mount', $Image)
    if ($Writable) { $arguments += '--writable' }
    $service = Get-Service -Name InocriumReflect -ErrorAction SilentlyContinue
    $reason = New-Object 'System.Collections.Generic.List[string]'
    if ($null -ne $service -and $service.Status -eq 'Running') {
        # --detach prevents main.rs from silently falling back to an indefinitely
        # running direct mount if the service pipe becomes unavailable.
        try { $result = Invoke-Inocrium -Exe $Exe -Arguments (@('--detach') + $arguments) -Service -TimeoutSeconds 240 }
        catch {
            $Workspace.CleanupBlockers.Add("Service mount completion is unknown for $Image; reconcile its mount id before removing this workspace.")
            throw
        }
        if ($result.ExitCode -eq 0) {
            $json = ConvertFrom-InocriumJson -Text $result.Stdout
            $ids = @(Get-InocriumJsonValues -Object $json -Names @('id', 'mount_id', 'mountId') |
                Where-Object { $_ -is [string] -and -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -Unique)
            if ($ids.Count -ne 1) {
                # Never use unmount all: that could destroy a user's unrelated mounts.
                $problem = "Service mount succeeded but returned no unambiguous mount id. Reconcile JSON and manually unmount this image: $Image. stdout: $($result.Stdout)"
                $Workspace.CleanupBlockers.Add($problem)
                throw $problem
            }
            $mount = [pscustomobject]@{ Mode = 'Service'; Id = $ids[0]; Handle = $null; Exe = $Exe; Letter = $null; Stopped = $false; PayloadName = $PayloadName }
            $Workspace.Mounts.Add($mount)
            $mount.Letter = Get-NewDriveLetter -Before $before -Json $json -PayloadName $PayloadName
            if ($mount.Letter) { return [pscustomobject]@{ Mount = $mount; Reason = $null } }
            Stop-TestImageMount -Mount $mount
            $reason.Add('Service mount exposed no accessible payload volume.')
        }
        else { $reason.Add("Service mount failed ($($result.ExitCode)): $($result.Stderr)") }
    }
    $handle = Invoke-Inocrium -Exe $Exe -Arguments $arguments -Background -Workspace $Workspace
    $mount = [pscustomobject]@{ Mode = 'Direct'; Id = $null; Handle = $handle; Exe = $Exe; Letter = $null; Stopped = $false; PayloadName = $PayloadName }
    $Workspace.Mounts.Add($mount)
    $mount.Letter = Get-NewDriveLetter -Before $before -PayloadName $PayloadName
    if ($mount.Letter -and -not $handle.Process.HasExited) { return [pscustomobject]@{ Mount = $mount; Reason = $null } }
    if ($handle.Process.HasExited) { $reason.Add("Direct mount exited ($($handle.Process.ExitCode)): $($handle.StderrTask.Result)") }
    else { $reason.Add('Direct mount exposed no accessible payload volume within 30 seconds.') }
    Stop-TestImageMount -Mount $mount
    return [pscustomobject]@{ Mount = $null; Reason = $reason -join ' ' }
}

function Stop-TestImageMount {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Mount)
    if ($Mount.Stopped) { return }
    if ($Mount.Mode -eq 'Service') {
        $result = Invoke-Inocrium -Exe $Mount.Exe -Arguments @('unmount', $Mount.Id) -Service -TimeoutSeconds 90
        if ($result.ExitCode -ne 0) { throw "Unmount failed: $($result.Stderr)" }
    }
    else { Stop-InocriumProcess -Handle $Mount.Handle }
    if ($Mount.Letter) {
        $payload = [IO.Path]::Combine($Mount.Letter + ':\', $Mount.PayloadName)
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        # Once the drive letter is gone, PowerShell path cmdlets fail with a missing drive; .NET does not.
        while ([IO.Directory]::Exists($payload)) {
            if ([DateTime]::UtcNow -ge $deadline) { throw "Image mount still accessible after unmount: $payload" }
            Start-Sleep -Milliseconds 250
        }
    }
    $Mount.Stopped = $true
}

function Remove-TestWorkspace {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Workspace)
    $path = Test-WorkspacePath -Workspace $Workspace -Path $Workspace.Path
    $problems = New-Object 'System.Collections.Generic.List[string]'
    foreach ($mount in $Workspace.Mounts) {
        try { Stop-TestImageMount -Mount $mount } catch { $problems.Add($_.Exception.Message) }
    }
    foreach ($handle in $Workspace.Processes) {
        try { Stop-InocriumProcess -Handle $handle } catch { $problems.Add($_.Exception.Message) }
    }
    foreach ($vhd in $Workspace.Vhds) {
        try { Remove-TestVhd -Vhd $vhd } catch { $problems.Add($_.Exception.Message) }
    }
    foreach ($blocker in $Workspace.CleanupBlockers) { $problems.Add($blocker) }
    if ($problems.Count -gt 0) { throw "Cleanup failed; preserved $path. $($problems -join ' ')" }
    if (Test-Path -LiteralPath $path) {
        foreach ($item in Get-ChildItem -LiteralPath $path -Recurse -Force) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing cleanup through a reparse point: $($item.FullName)" }
        }
        Remove-Item -LiteralPath $path -Recurse -Force
    }
}

Export-ModuleMember -Function Test-Administrator, New-TestWorkspace, New-TestVhd, Get-TestDisk,
    Initialize-TestDisk, Write-RandomFiles, Get-FileHashes, Compare-FileHashes, Invoke-Inocrium,
    ConvertFrom-InocriumJson, Get-NewDriveLetter, Get-TestPartitionRoot, Remove-TestVhd,
    Start-TestImageMount, Stop-TestImageMount, Remove-TestWorkspace
