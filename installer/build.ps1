[CmdletBinding()]
param(
    [ValidateSet('Release', 'Debug')]
    [string]$Configuration = 'Release',
    [ValidateSet('x64', 'ARM64')]
    [string]$Platform = 'x64',
    [string]$Version = '0.1.0'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# MSI uses three numeric fields, with Windows Installer's field limits.
if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)$' -or
    [uint64]$Matches[1] -gt 255 -or [uint64]$Matches[2] -gt 255 -or
    [uint64]$Matches[3] -gt 65535) {
    throw 'Version must be major.minor.patch (major/minor <= 255, patch <= 65535).'
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $PSScriptRoot 'out'
$stagingRoot = Join-Path $outputRoot "staging\$Platform"
$guiPublishDir = Join-Path $stagingRoot 'gui'
$rustTarget = if ($Platform -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$runtimeId = if ($Platform -eq 'ARM64') { 'win-arm64' } else { 'win-x64' }

Get-Command cargo, dotnet -ErrorAction Stop | Out-Null
Push-Location $repoRoot
try {
    # Isolate Rust artifacts from any caller's CARGO_TARGET_DIR setting.
    $cargoTargetDir = Join-Path $repoRoot 'target'
    & cargo build --release -p inocrium-service -p inocrium --target $rustTarget --target-dir $cargoTargetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }
    $rustOutDir = Join-Path $cargoTargetDir "$rustTarget\release"

    # Remove stale files only from this architecture's verified staging directory.
    $resolvedStagingRoot = [IO.Path]::GetFullPath($stagingRoot)
    $resolvedOutputRoot = [IO.Path]::GetFullPath($outputRoot).TrimEnd('\') + '\'
    if (-not $resolvedStagingRoot.StartsWith($resolvedOutputRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Staging directory is outside installer/out: $resolvedStagingRoot"
    }
    if (Test-Path -LiteralPath $resolvedStagingRoot) {
        Remove-Item -LiteralPath $resolvedStagingRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $guiPublishDir -Force | Out-Null

    # VS MSBuild supplies PRI/XAML tasks that can be missing from bare dotnet SDK installs.
    # Use its environment for dotnet publish when available (CI uses setup-msbuild).
    $msbuildCommand = Get-Command msbuild -ErrorAction SilentlyContinue
    if ($null -ne $msbuildCommand) {
        $vsToolsPath = Split-Path -Parent $msbuildCommand.Source
        $publishToolsArguments = @("-p:MSBuildExtensionsPath=$vsToolsPath")
    } else {
        $publishToolsArguments = @()
    }
    & dotnet publish (Join-Path $repoRoot 'gui\Inocrium.App\Inocrium.App.csproj') `
        -c $Configuration -r $runtimeId --self-contained true -o $guiPublishDir `
        "-p:Platform=$Platform" '-p:WindowsAppSDKSelfContained=true' @publishToolsArguments
    if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed with exit code $LASTEXITCODE." }

    Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination (Join-Path $guiPublishDir 'LICENSE')
    & dotnet build (Join-Path $PSScriptRoot 'Inocrium.Installer.wixproj') `
        -c $Configuration "-p:Platform=$Platform" "-p:ProductVersion=$Version" `
        "-p:RustOutDir=$rustOutDir" "-p:GuiPublishDir=$guiPublishDir"
    if ($LASTEXITCODE -ne 0) { throw "WiX build failed with exit code $LASTEXITCODE." }

    $msiPath = Join-Path $outputRoot "$Platform\inocrium-reflect-$Version-$Platform.msi"
    if (-not (Test-Path -LiteralPath $msiPath)) { throw "Expected MSI was not produced: $msiPath" }
    Write-Output "Installer: $msiPath"
} finally {
    Pop-Location
}
