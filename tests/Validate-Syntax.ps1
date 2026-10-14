#Requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$failed = $false
$files = @(Get-ChildItem -LiteralPath $PSScriptRoot -Recurse -File |
    Where-Object { $_.Extension -in @('.ps1', '.psm1') })
foreach ($file in $files) {
    $tokens = $null
    $parseErrors = $null
    $null = [System.Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count -gt 0) {
        $failed = $true
        foreach ($parseError in $parseErrors) {
            Write-Host ('{0}:{1}:{2}: {3}' -f $file.Name, $parseError.Extent.StartLineNumber,
                $parseError.Extent.StartColumnNumber, $parseError.Message) -ForegroundColor Red
        }
    }
    else { Write-Host ('Parsed {0} (PowerShell {1})' -f $file.Name, $PSVersionTable.PSVersion) }
}

if (Get-Module -ListAvailable PSScriptAnalyzer) {
    Import-Module PSScriptAnalyzer
    $findings = @(Invoke-ScriptAnalyzer -Path $PSScriptRoot -Recurse -Severity Warning, Error)
    foreach ($finding in $findings) {
        Write-Host ('{0}:{1}: {2}: {3}' -f $finding.ScriptName, $finding.Line, $finding.RuleName, $finding.Message)
    }
    if ($findings.Count -gt 0) { $failed = $true }
    else { Write-Host 'PSScriptAnalyzer: no warnings or errors.' }
}
else { Write-Host 'PSScriptAnalyzer is not installed; analyzer check skipped.' }

if ($failed) { exit 1 }
exit 0
