#Requires -Version 5.1
[CmdletBinding()]
param(
    [string] $InocriumExe,
    [string[]] $Tag = @(),
    [string[]] $ExcludeTag = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Administrator is required. Open an elevated PowerShell and run tests\Run-AdminTests.ps1 again.' -ForegroundColor Red
    exit 1
}

$pester = Get-Module -ListAvailable Pester | Where-Object { $_.Version.Major -ge 5 } |
    Sort-Object Version -Descending | Select-Object -First 1
if ($null -eq $pester) {
    Write-Host 'Pester 5 or newer is required. Run: Install-Module Pester -MinimumVersion 5 -Scope CurrentUser' -ForegroundColor Red
    exit 1
}

try {
    Import-Module $pester.Path -Force
    $repoRoot = Split-Path -Parent $PSScriptRoot
    if ([string]::IsNullOrWhiteSpace($InocriumExe)) {
        $InocriumExe = @('target\release\inocrium.exe', 'target\debug\inocrium.exe') |
            ForEach-Object { Join-Path $repoRoot $_ } | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
        if ([string]::IsNullOrWhiteSpace($InocriumExe)) {
            throw 'CLI not found. Build inocrium.exe or pass -InocriumExe with its path.'
        }
    }
    $resolvedExe = (Resolve-Path -LiteralPath $InocriumExe).ProviderPath
    $containers = @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.Tests.ps1' -File | ForEach-Object {
        New-PesterContainer -Path $_.FullName -Data @{ InocriumExe = $resolvedExe }
    })
    if ($containers.Count -eq 0) { throw 'No *.Tests.ps1 files found.' }
    $configuration = New-PesterConfiguration
    $configuration.Run.Container = $containers
    $configuration.Run.PassThru = $true
    $configuration.Output.Verbosity = 'Detailed'
    $configuration.Filter.Tag = $Tag
    $configuration.Filter.ExcludeTag = $ExcludeTag
    $result = Invoke-Pester -Configuration $configuration
    if ($result.Result -eq 'Failed' -or $result.FailedCount -gt 0 -or $result.FailedContainersCount -gt 0) { exit 1 }
    exit 0
}
catch {
    Write-Host $_.Exception.Message -ForegroundColor Red
    exit 1
}
