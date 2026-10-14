#Requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Inocrium.TestHelpers.psm1') -Force

function Test-HelperAssertion {
    param([bool] $Condition, [string] $Message)
    if (-not $Condition) { throw $Message }
}

# No elevation, Storage cmdlets, VHDs, or real inocrium.exe are used here.
$workspace = [pscustomobject]@{
    Path = Join-Path (Join-Path $env:TEMP 'inocrium-tests') ([guid]::NewGuid().ToString('D'))
    Vhds = New-Object 'System.Collections.Generic.List[object]'
    Mounts = New-Object 'System.Collections.Generic.List[object]'
    Processes = New-Object 'System.Collections.Generic.List[object]'
    CleanupBlockers = New-Object 'System.Collections.Generic.List[string]'
}
$null = New-Item -ItemType Directory -Path $workspace.Path -Force
try {
    $exe = Join-Path $workspace.Path 'fake cli.exe'
    Add-Type -OutputAssembly $exe -OutputType ConsoleApplication -TypeDefinition @'
using System;
using System.IO;
using System.Threading;
using System.Runtime.InteropServices;
public static class FakeInocrium {
    private delegate bool Handler(uint signal);
    private static volatile bool stopped;
    private static Handler handler = delegate(uint signal) { stopped = true; return true; };
    [DllImport("kernel32.dll", SetLastError = true)] private static extern bool SetConsoleCtrlHandler(Handler handler, bool add);
    public static int Main(string[] args) {
        int recordIndex = Array.IndexOf(args, "--record");
        string record = args[recordIndex + 1];
        File.WriteAllLines(record, args);
        File.WriteAllText(record + ".password", Environment.GetEnvironmentVariable("INOCRIUM_PASSWORD") ?? "<absent>");
        if (Array.IndexOf(args, "--hold") >= 0) {
            if (!SetConsoleCtrlHandler(handler, true)) return 2;
            File.WriteAllText(record + ".ready", "ready");
            while (!stopped) Thread.Sleep(100);
            File.WriteAllText(record + ".stopped", "graceful cleanup");
        }
        Console.Out.WriteLine("{\"id\":\"11111111-1111-1111-1111-111111111111\",\"image\":\"test.irimg\",\"disk\":0,\"partition\":null,\"writable\":false,\"diskNumber\":7,\"letters\":[\"Z:\"]}");
        Console.Error.WriteLine("{\"type\":\"progress\",\"progress\":{}}");
        return 0;
    }
}
'@
    $record = Join-Path $workspace.Path 'argument record.txt'
    $values = @('space value', 'quote"value', 'trailing\', '', 'slashes\\"quote')
    $result = Invoke-Inocrium -Exe $exe -Arguments (@('--record', $record) + $values) -Password 'synthetic-test'
    Test-HelperAssertion ($result.ExitCode -eq 0) 'Fake CLI invocation failed.'
    $actual = [IO.File]::ReadAllLines($record)
    $expected = @('--json', '--direct', '--record', $record) + $values
    Test-HelperAssertion ($actual.Length -eq $expected.Length) 'Native argument count changed.'
    for ($index = 0; $index -lt $expected.Length; $index++) {
        Test-HelperAssertion ($actual[$index] -ceq $expected[$index]) "Native argument $index changed."
    }
    Test-HelperAssertion ([IO.File]::ReadAllText($record + '.password') -eq 'synthetic-test') 'Child password injection failed.'
    $json = ConvertFrom-InocriumJson -Text $result.Stdout
    Test-HelperAssertion ($json.letters[0] -eq 'Z:') 'Current CLI mount schema did not parse.'
    Test-HelperAssertion ($json.diskNumber -eq 7 -and $json.disk -eq 0 -and $null -eq $json.partition) 'Mount disk/partition fields did not parse.'
    $json = ConvertFrom-InocriumJson -Text '{"image":"C:\\images\\test.irimg"}'
    Test-HelperAssertion ($json.image -eq 'C:\images\test.irimg') 'Direct backup image result did not parse.'
    $json = ConvertFrom-InocriumJson -Text '{"verified":true,"image":"test.irimg"}'
    Test-HelperAssertion ($json.verified -and $json.image -eq 'test.irimg') 'Direct verify result did not parse.'
    $json = ConvertFrom-InocriumJson -Text '"11111111-1111-1111-1111-111111111111"'
    Test-HelperAssertion ($json -eq '11111111-1111-1111-1111-111111111111') 'Definition-create UUID result did not parse.'
    $json = ConvertFrom-InocriumJson -Text '[]'
    Test-HelperAssertion ($json -is [array] -and $json.Count -eq 0) 'Empty CLI arrays must remain arrays.'
    $json = ConvertFrom-InocriumJson -Text '[{"number":7,"diskId":"0000ABCD"}]'
    Test-HelperAssertion ($json -is [array] -and $json.Count -eq 1 -and $json[0].diskId -eq '0000ABCD') 'Single-item CLI arrays must remain arrays.'
    foreach ($arguments in @(@('--password', 'synthetic-test'), @('--password=synthetic-test'))) {
        $caught = $false
        try { $null = Invoke-Inocrium -Exe $exe -Arguments $arguments } catch {
            $caught = $_.Exception.Message -eq 'Passwords must never be passed as arguments.'
        }
        Test-HelperAssertion $caught 'Password-valued command arguments were not rejected.'
    }
    $json = ConvertFrom-InocriumJson -Text ('{"jobId":"first"}' + "`n" + '{"state":"succeeded"}' + "`nprogress text`n42`nnull")
    Test-HelperAssertion ($json.state -eq 'succeeded') 'JSON lines did not select the final structured result.'
    Test-HelperAssertion ($null -eq (ConvertFrom-InocriumJson -Text 'not json')) 'Invalid JSON was accepted.'
    $null = Invoke-Inocrium -Exe $exe -Arguments @('--record', $record)
    Test-HelperAssertion ([IO.File]::ReadAllText($record + '.password') -eq '<absent>') 'Child unexpectedly inherited a password.'

    $handle = Invoke-Inocrium -Exe $exe -Arguments @('--record', $record, '--hold') -Background -Workspace $workspace
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath ($record + '.ready'))) {
        if ($handle.Process.HasExited) { throw "Private console host exited: $($handle.StderrTask.Result)" }
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Private console fake CLI did not become ready.' }
        Start-Sleep -Milliseconds 100
    }
    # Use the same shutdown path as a direct image mount.
    & (Get-Module Inocrium.TestHelpers) { param($h) Stop-InocriumProcess -Handle $h } $handle
    Test-HelperAssertion (Test-Path -LiteralPath ($record + '.stopped')) 'Ctrl+C did not execute child cleanup.'
    Test-HelperAssertion ((ConvertFrom-InocriumJson -Text $handle.StdoutTask.Result).id -eq '11111111-1111-1111-1111-111111111111') 'Host did not forward CLI stdout.'

    # Inject detach failure without calling Storage cmdlets: cleanup must attempt
    # the next resource, preserve the workspace, and succeed on a later retry.
    $module = Get-Module Inocrium.TestHelpers
    $original = & $module { (Get-Item Function:Remove-TestVhd).ScriptBlock }
    $first = [pscustomobject]@{ Attempts = 0; Fail = $true }
    $second = [pscustomobject]@{ Attempts = 0; Fail = $false }
    $workspace.Vhds.Add($first)
    $workspace.Vhds.Add($second)
    try {
        & $module {
            Set-Item Function:script:Remove-TestVhd -Value {
                param($Vhd)
                $Vhd.Attempts++
                if ($Vhd.Fail) { throw 'synthetic detach failure' }
            }
        }
        $caught = $false
        try { Remove-TestWorkspace -Workspace $workspace } catch { $caught = $_.Exception.Message -match 'synthetic detach failure' }
        Test-HelperAssertion $caught 'Cleanup did not report a detach failure.'
        Test-HelperAssertion ($first.Attempts -eq 1 -and $second.Attempts -eq 1) 'Cleanup stopped before attempting all resources.'
        Test-HelperAssertion (Test-Path -LiteralPath $workspace.Path) 'Cleanup deleted a workspace after detach failure.'
    }
    finally {
        & $module { param($body) Set-Item Function:script:Remove-TestVhd -Value $body } $original
        $workspace.Vhds.Clear()
    }
    $workspace.CleanupBlockers.Add('synthetic unknown service mount')
    $caught = $false
    try { Remove-TestWorkspace -Workspace $workspace } catch { $caught = $_.Exception.Message -match 'synthetic unknown service mount' }
    Test-HelperAssertion ($caught -and (Test-Path -LiteralPath $workspace.Path)) 'Unknown service mount did not preserve its workspace.'
    $workspace.CleanupBlockers.Clear()
    Write-Host 'Helper checks passed: native quoting, child password, CLI JSON/JSON lines, graceful private-console shutdown, and failure cleanup.'
}
finally { Remove-TestWorkspace -Workspace $workspace }
