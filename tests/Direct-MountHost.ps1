#Requires -Version 5.1
[CmdletBinding()]
param([Parameter(Mandatory)][string] $ConfigPath)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# This helper owns a private hidden console. Its child CLI receives Ctrl+C,
# so the Rust mount manager can remove letters, iSCSI sessions and COW files.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class InocriumTestConsole {
    private delegate bool Handler(uint signal);
    private static Handler handler = delegate(uint signal) { return true; };
    [DllImport("kernel32.dll", SetLastError = true)] private static extern bool AllocConsole();
    [DllImport("kernel32.dll")] private static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] private static extern bool SetConsoleCtrlHandler(Handler handler, bool add);
    [DllImport("kernel32.dll")] private static extern IntPtr GetConsoleWindow();
    [DllImport("user32.dll")] private static extern bool ShowWindow(IntPtr window, int show);
    [DllImport("kernel32.dll", SetLastError = true)] private static extern bool GenerateConsoleCtrlEvent(uint signal, uint group);
    public static void Initialize() {
        // Windows PowerShell may attach a console even with CREATE_NO_WINDOW.
        // Detach this host only, then allocate the console owned by this mount.
        FreeConsole();
        if (!AllocConsole()) throw new System.ComponentModel.Win32Exception();
        ShowWindow(GetConsoleWindow(), 0);
        if (!SetConsoleCtrlHandler(handler, true)) throw new System.ComponentModel.Win32Exception();
    }
    public static void Stop() {
        if (!GenerateConsoleCtrlEvent(0, 0)) throw new System.ComponentModel.Win32Exception();
    }
}
'@

$config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
# Preserve the parent's redirected pipes before AllocConsole replaces Win32
# standard handles. The CLI gets its own redirected pipes through Invoke-Inocrium.
$stdout = New-Object IO.StreamWriter([Console]::OpenStandardOutput(), (New-Object Text.UTF8Encoding($false)))
$stderr = New-Object IO.StreamWriter([Console]::OpenStandardError(), (New-Object Text.UTF8Encoding($false)))
# Remove the transient configuration (which may contain a synthetic password).
Remove-Item -LiteralPath $ConfigPath -Force
[InocriumTestConsole]::Initialize()
Import-Module (Join-Path $PSScriptRoot 'Inocrium.TestHelpers.psm1') -Force
$workspace = [pscustomobject]@{ Processes = New-Object 'System.Collections.Generic.List[object]' }
$parameters = @{ Exe = [string]$config.Exe; Arguments = [string[]]$config.Arguments
    Background = $true; Workspace = $workspace; ConsoleHost = $true }
if ($config.PSObject.Properties['Password']) { $parameters.Password = [string]$config.Password }
$handle = Invoke-Inocrium @parameters
while (-not $handle.Process.WaitForExit(200)) {
    if (Test-Path -LiteralPath $config.StopPath) {
        [InocriumTestConsole]::Stop()
        # Repeat only on explicit retries from the parent cleanup.
        Remove-Item -LiteralPath $config.StopPath -Force
    }
}
$code = $handle.Process.ExitCode
$stdout.Write($handle.StdoutTask.Result)
$stderr.Write($handle.StderrTask.Result)
$stdout.Dispose()
$stderr.Dispose()
$handle.Process.Dispose()
exit $code
