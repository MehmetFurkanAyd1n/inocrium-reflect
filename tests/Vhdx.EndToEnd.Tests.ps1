#Requires -Version 5.1
param([string] $InocriumExe)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Describe 'inocrium VHDX end-to-end scenarios' -Tag 'Admin', 'Slow' {
    BeforeAll {
        Import-Module (Join-Path $PSScriptRoot 'Inocrium.TestHelpers.psm1') -Force
        if (-not (Test-Administrator)) { throw 'Run this suite through Run-AdminTests.ps1 in an elevated PowerShell.' }
        if ([string]::IsNullOrWhiteSpace($InocriumExe) -or -not (Test-Path -LiteralPath $InocriumExe -PathType Leaf)) {
            throw 'Pass InocriumExe through a Pester container, or use Run-AdminTests.ps1.'
        }
        $script:Exe = $InocriumExe
        $script:Workspaces = New-Object 'System.Collections.Generic.List[object]'
        $script:Workspace = $null

        function Invoke-CheckedInocrium {
            param([string[]] $Arguments, [string] $Password)
            $parameters = @{ Exe = $script:Exe; Arguments = $Arguments }
            if ($PSBoundParameters.ContainsKey('Password')) { $parameters.Password = $Password }
            $result = Invoke-Inocrium @parameters
            $result.ExitCode | Should -Be 0 -Because "CLI command must succeed. stdout: $($result.Stdout) stderr: $($result.Stderr)"
            if ($Arguments[0] -eq 'verify') {
                $json = ConvertFrom-InocriumJson -Text $result.Stdout
                $json.verified | Should -BeTrue
                $json.image | Should -Be $Arguments[1]
            }
            return $result
        }

        function New-ScenarioFixture {
            param([string] $Style = 'GPT', [long] $SizeBytes = 1GB, [switch] $Fat32,
                [long[]] $FileSizes = @(4096, 1MB, 50MB, 32MB))
            $vhd = New-TestVhd -Workspace $script:Workspace -SizeBytes $SizeBytes
            $parts = @(Initialize-TestDisk -Vhd $vhd -PartitionStyle $Style -IncludeFat32:$Fat32)
            $payload = 'payload-' + [guid]::NewGuid().ToString('N')
            $expected = @{}
            foreach ($part in $parts) {
                $sizes = if ($part.FileSystem -eq 'FAT32') { @(4096, 2MB) } else { $FileSizes }
                $root = Join-Path ($part.Letter + ':\') $payload
                $expected[[int]$part.Number] = Write-RandomFiles -Root $root -Sizes $sizes
            }
            [pscustomobject]@{ Vhd = $vhd; Partitions = $parts; Payload = $payload; Expected = $expected }
        }

        function New-ScenarioBackup {
            param([Parameter(Mandatory)] $Fixture, [string[]] $Options = @(),
                [string] $Prefix = 'scenario', [string] $Password)
            $destination = Join-Path $script:Workspace.Path 'images'
            $null = New-Item -ItemType Directory -Path $destination -Force
            $before = @(Get-ChildItem -LiteralPath $destination -Filter '*.irimg' -File | ForEach-Object { $_.FullName })
            $disk = Get-TestDisk -Vhd $Fixture.Vhd
            $arguments = @('backup', '--disk', [string]$disk.Number, '--dest', $destination, '--prefix', $Prefix) + $Options
            $parameters = @{ Arguments = $arguments }
            if ($PSBoundParameters.ContainsKey('Password')) { $parameters.Password = $Password }
            $result = Invoke-CheckedInocrium @parameters
            $json = ConvertFrom-InocriumJson -Text $result.Stdout
            $images = @(Get-ChildItem -LiteralPath $destination -Filter '*.irimg' -File |
                Where-Object { $_.FullName -notin $before })
            $images.Count | Should -Be 1 -Because 'Each successful backup must create one new primary image.'
            $json.image | Should -Be $images[0].FullName
            $images[0].Name | Should -Match ('^' + [regex]::Escape($Prefix) + '-.+\.irimg$')
            (Test-Path -LiteralPath ($images[0].FullName + '.ckpt')) | Should -BeFalse
            return $images[0].FullName
        }

        function Test-RestoredFixture {
            param([Parameter(Mandatory)] $Fixture, [Parameter(Mandatory)] $Target, [switch] $Extended)
            $disk = Get-TestDisk -Vhd $Target
            $disk.PartitionStyle | Should -Be $(if ($Fixture.Vhd.Path) { $Fixture.Style } else { '' })
            $actualParts = @(Get-Partition -DiskNumber $disk.Number | Sort-Object PartitionNumber)
            $actualParts.Count | Should -Be $Fixture.Layout.Count
            foreach ($original in $Fixture.Layout) {
                $actual = @($actualParts | Where-Object { $_.PartitionNumber -eq $original.PartitionNumber })
                $actual.Count | Should -Be 1
                $actual[0].Offset | Should -Be $original.Offset
                $payloadPart = @($Fixture.Partitions | Where-Object { $_.Number -eq $original.PartitionNumber })
                if ($Extended -and $payloadPart.Count -eq 1 -and $payloadPart[0].FileSystem -eq 'NTFS') {
                    $actual[0].Size | Should -BeGreaterThan $original.Size
                }
                else { $actual[0].Size | Should -Be $original.Size }
            }
            foreach ($part in $Fixture.Partitions) {
                $root = Get-TestPartitionRoot -Vhd $Target -PartitionNumber $part.Number
                $volume = Get-Volume -DriveLetter $root.Substring(0, 1)
                $volume.FileSystem | Should -Be $part.FileSystem
                $actual = Get-FileHashes -Root (Join-Path $root $Fixture.Payload)
                Compare-FileHashes -Expected $Fixture.Expected[[int]$part.Number] -Actual $actual | Should -BeTrue
            }
        }

        function Save-FixtureLayout {
            param([Parameter(Mandatory)] $Fixture)
            $disk = Get-TestDisk -Vhd $Fixture.Vhd
            $Fixture | Add-Member -NotePropertyName Style -NotePropertyValue ([string]$disk.PartitionStyle)
            $Fixture | Add-Member -NotePropertyName Layout -NotePropertyValue @(Get-Partition -DiskNumber $disk.Number |
                Select-Object PartitionNumber, Offset, Size)
        }

        function New-MountFixture {
            $fixture = New-ScenarioFixture -SizeBytes 256MB -FileSizes @(4096, 1MB, 4MB)
            $image = New-ScenarioBackup -Fixture $fixture -Options @('--compress', '3', '--verify')
            Remove-TestVhd -Vhd $fixture.Vhd
            [pscustomobject]@{ Fixture = $fixture; Image = $image }
        }
    }

    BeforeEach {
        $script:Workspace = $null
        $script:Workspace = New-TestWorkspace
        $script:Workspaces.Add($script:Workspace)
    }

    AfterEach {
        if ((Get-Variable -Name Workspace -Scope Script -ErrorAction SilentlyContinue) -and $null -ne $script:Workspace) {
            Remove-TestWorkspace -Workspace $script:Workspace
        }
    }

    AfterAll {
        # Retry every registered workspace, even when setup/test/AfterEach failed.
        $failures = @()
        if (Get-Variable -Name Workspaces -Scope Script -ErrorAction SilentlyContinue) {
            foreach ($workspace in $script:Workspaces) {
                try { Remove-TestWorkspace -Workspace $workspace } catch { $failures += $_.Exception.Message }
            }
        }
        if ($failures.Count -gt 0) { throw ($failures -join [Environment]::NewLine) }
    }

    It 'GPT smart backup verifies and restores onto a larger disk, extending NTFS' {
        $fixture = New-ScenarioFixture -Fat32 -SizeBytes 1GB
        Save-FixtureLayout -Fixture $fixture
        $null = Invoke-CheckedInocrium -Arguments @('disks')
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--compress', '3', '--verify')
        $null = Invoke-CheckedInocrium -Arguments @('verify', $image)
        $target = New-TestVhd -Workspace $script:Workspace -SizeBytes 2GB
        $targetDisk = Get-TestDisk -Vhd $target
        $null = Invoke-CheckedInocrium -Arguments @('restore', $image, '--target-disk', [string]$targetDisk.Number, '--extend', '--yes')
        Test-RestoredFixture -Fixture $fixture -Target $target -Extended
        $ntfs = @($fixture.Partitions | Where-Object { $_.FileSystem -eq 'NTFS' })[0]
        $root = Get-TestPartitionRoot -Vhd $target -PartitionNumber $ntfs.Number
        # Check filesystem capacity too, not just the partition table.
        (Get-Volume -DriveLetter $root.Substring(0, 1)).Size | Should -BeGreaterThan $ntfs.Size
    }

    It 'MBR forensic backup round trips onto a same-size disk' {
        $fixture = New-ScenarioFixture -Style MBR -SizeBytes 256MB -FileSizes @(4096, 1MB, 50MB)
        Save-FixtureLayout -Fixture $fixture
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--forensic', '--compress', '3')
        $null = Invoke-CheckedInocrium -Arguments @('verify', $image)
        $target = New-TestVhd -Workspace $script:Workspace -SizeBytes 256MB
        $disk = Get-TestDisk -Vhd $target
        $null = Invoke-CheckedInocrium -Arguments @('restore', $image, '--target-disk', [string]$disk.Number, '--yes')
        Test-RestoredFixture -Fixture $fixture -Target $target
    }

    It 'encrypted 64M split images verify only with the correct password' {
        $fixture = New-ScenarioFixture
        $password = 'inocrium-test-' + [guid]::NewGuid().ToString('N')
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--encrypt', '--split', '64M', '--compress', '3') -Password $password
        $baseName = [IO.Path]::GetFileNameWithoutExtension($image)
        $parts = @(Get-ChildItem -LiteralPath (Split-Path -Parent $image) -File |
            Where-Object { $_.Name -match ('^' + [regex]::Escape($baseName) + '\.i[0-9]{2,}$') })
        $parts.Count | Should -BeGreaterThan 0 -Because 'More than 64 MiB of incompressible payload must split.'
        $null = Invoke-CheckedInocrium -Arguments @('verify', $image) -Password $password
        $wrong = Invoke-Inocrium -Exe $script:Exe -Arguments @('verify', $image) -Password ($password + '-wrong')
        $wrong.ExitCode | Should -Not -Be 0
        $null = Invoke-CheckedInocrium -Arguments @('verify', $image) -Password $password
    }

    It 'verify detects bytes flipped in the middle of an image' {
        $fixture = New-ScenarioFixture
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--compress', '3')
        $null = Invoke-CheckedInocrium -Arguments @('verify', $image)
        $stream = [IO.File]::Open($image, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        try {
            $offset = [long][Math]::Floor($stream.Length / 2)
            $null = $stream.Seek($offset, [IO.SeekOrigin]::Begin)
            $bytes = New-Object byte[] 32
            $stream.Read($bytes, 0, $bytes.Length) | Should -Be $bytes.Length
            for ($index = 0; $index -lt $bytes.Length; $index++) { $bytes[$index] = $bytes[$index] -bxor 0xA5 }
            $null = $stream.Seek($offset, [IO.SeekOrigin]::Begin)
            $stream.Write($bytes, 0, $bytes.Length)
            $stream.Flush($true)
        }
        finally { $stream.Dispose() }
        $result = Invoke-Inocrium -Exe $script:Exe -Arguments @('verify', $image)
        $result.ExitCode | Should -Not -Be 0
    }

    It 'a read-only mount exposes matching files and rejects writes' {
        $source = New-MountFixture
        $attempt = Start-TestImageMount -Workspace $script:Workspace -Exe $script:Exe -Image $source.Image -PayloadName $source.Fixture.Payload
        if ($null -eq $attempt.Mount) { Set-ItResult -Skipped -Because $attempt.Reason; return }
        $mount = $attempt.Mount
        try {
            $root = Join-Path ($mount.Letter + ':\') $source.Fixture.Payload
            $part = $source.Fixture.Partitions[0]
            Compare-FileHashes -Expected $source.Fixture.Expected[[int]$part.Number] -Actual (Get-FileHashes -Root $root) | Should -BeTrue
            $writePath = Join-Path $root 'must-not-write.txt'
            { [IO.File]::WriteAllText($writePath, 'read-only probe') } | Should -Throw
            (Test-Path -LiteralPath $writePath) | Should -BeFalse
        }
        finally { Stop-TestImageMount -Mount $mount }
    }

    It 'a writable mount discards changes when remounted read-only' {
        $source = New-MountFixture
        $attempt = Start-TestImageMount -Workspace $script:Workspace -Exe $script:Exe -Image $source.Image -PayloadName $source.Fixture.Payload -Writable
        if ($null -eq $attempt.Mount) { Set-ItResult -Skipped -Because $attempt.Reason; return }
        $mount = $attempt.Mount
        $part = $source.Fixture.Partitions[0]
        $probeName = 'discard-me.txt'
        try {
            $root = Join-Path ($mount.Letter + ':\') $source.Fixture.Payload
            Compare-FileHashes -Expected $source.Fixture.Expected[[int]$part.Number] -Actual (Get-FileHashes -Root $root) | Should -BeTrue
            $probe = Join-Path $root $probeName
            [IO.File]::WriteAllText($probe, 'temporary writable overlay')
            [IO.File]::ReadAllText($probe) | Should -Be 'temporary writable overlay'
        }
        finally { Stop-TestImageMount -Mount $mount }
        $attempt = Start-TestImageMount -Workspace $script:Workspace -Exe $script:Exe -Image $source.Image -PayloadName $source.Fixture.Payload
        if ($null -eq $attempt.Mount) { throw ('Read-only remount failed after a successful writable mount: ' + $attempt.Reason) }
        $mount = $attempt.Mount
        try {
            $root = Join-Path ($mount.Letter + ':\') $source.Fixture.Payload
            (Test-Path -LiteralPath (Join-Path $root $probeName)) | Should -BeFalse
            Compare-FileHashes -Expected $source.Fixture.Expected[[int]$part.Number] -Actual (Get-FileHashes -Root $root) | Should -BeTrue
        }
        finally { Stop-TestImageMount -Mount $mount }
    }

    It 'VHDX export attaches read-only and preserves both filesystems and hashes' {
        $fixture = New-ScenarioFixture -Fat32
        Save-FixtureLayout -Fixture $fixture
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--compress', '3')
        Remove-TestVhd -Vhd $fixture.Vhd
        $output = Join-Path $script:Workspace.Path 'export.vhdx'
        $null = Invoke-CheckedInocrium -Arguments @('export-vhdx', $image, $output)
        (Test-Path -LiteralPath $output -PathType Leaf) | Should -BeTrue
        $export = New-TestVhd -Workspace $script:Workspace -ExistingPath $output -ReadOnly
        (Get-TestDisk -Vhd $export).IsReadOnly | Should -BeTrue
        Test-RestoredFixture -Fixture $fixture -Target $export
    }

    It 'three backups with keep-last 2 retain exactly the newest two images' {
        $fixture = New-ScenarioFixture -SizeBytes 256MB -FileSizes @(4096, 1MB)
        $created = @()
        for ($index = 0; $index -lt 3; $index++) {
            # Distinct timestamps even when a small backup completes within a second.
            if ($index -gt 0) { Start-Sleep -Milliseconds 1100 }
            $created += New-ScenarioBackup -Fixture $fixture -Prefix 'retention' -Options @('--compress', '3', '--keep-last', '2')
        }
        $remaining = @(Get-ChildItem -LiteralPath (Join-Path $script:Workspace.Path 'images') -Filter '*.irimg' -File)
        $remaining.Count | Should -Be 2
        (Test-Path -LiteralPath $created[0]) | Should -BeFalse
        foreach ($image in $created[1..2]) {
            (Test-Path -LiteralPath $image) | Should -BeTrue
            $null = Invoke-CheckedInocrium -Arguments @('verify', $image)
        }
    }

    It 'a selected source partition restores into an existing target partition without changing its neighbor' {
        $fixture = New-ScenarioFixture -SizeBytes 256MB -FileSizes @(4096, 1MB, 50MB)
        $part = $fixture.Partitions[0]
        $image = New-ScenarioBackup -Fixture $fixture -Options @('--partitions', [string]$part.Number, '--compress', '3')
        $target = New-TestVhd -Workspace $script:Workspace -SizeBytes 1GB
        $disk = Get-TestDisk -Vhd $target
        $null = Initialize-Disk -Number $disk.Number -PartitionStyle GPT
        $neighbor = New-Partition -DiskNumber $disk.Number -Size 64MB -AssignDriveLetter
        $null = Format-Volume -Partition $neighbor -FileSystem NTFS -NewFileSystemLabel 'IR_KEEP' -Confirm:$false
        $neighborPayload = Join-Path ($neighbor.DriveLetter + ':\') 'neighbor-payload'
        $neighborHashes = Write-RandomFiles -Root $neighborPayload -Sizes @(4096, 2MB)
        $destination = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
        $null = Format-Volume -Partition $destination -FileSystem NTFS -NewFileSystemLabel 'IR_TARGET' -Confirm:$false
        $destination.Size | Should -BeGreaterThan $part.Size
        $before = @(Get-Partition -DiskNumber $disk.Number | Select-Object PartitionNumber, Offset, Size)
        # --disk is image-local (0); --target-disk is a Windows disk number.
        $disk = Get-TestDisk -Vhd $target
        $null = Invoke-CheckedInocrium -Arguments @('restore', $image, '--target-disk', [string]$disk.Number,
            '--disk', '0', '--partition', [string]$part.Number, '--target-partition', [string]$destination.PartitionNumber, '--yes')
        $root = Get-TestPartitionRoot -Vhd $target -PartitionNumber $destination.PartitionNumber
        Compare-FileHashes -Expected $fixture.Expected[[int]$part.Number] -Actual (Get-FileHashes -Root (Join-Path $root $fixture.Payload)) | Should -BeTrue
        $neighborRoot = Get-TestPartitionRoot -Vhd $target -PartitionNumber $neighbor.PartitionNumber
        Compare-FileHashes -Expected $neighborHashes -Actual (Get-FileHashes -Root (Join-Path $neighborRoot 'neighbor-payload')) | Should -BeTrue
        @(Get-Partition -DiskNumber $disk.Number).Count | Should -Be $before.Count
        foreach ($original in $before) {
            $actual = Get-Partition -DiskNumber $disk.Number -PartitionNumber $original.PartitionNumber
            $actual.Offset | Should -Be $original.Offset
            $actual.Size | Should -Be $original.Size
        }
    }
}
