#Requires -Version 5.1
<#
.SYNOPSIS
Runs one inocrium `mount` or `unmount` command elevated, from a non-elevated shell.

.DESCRIPTION
The inocrium service pipe accepts only Administrators and SYSTEM, so every CLI call must run
elevated. This helper validates the arguments, writes them to request.json in a new folder under
%TEMP%, and starts itself elevated (one UAC prompt) with only -File and -RequestPath on the
command line. The elevated copy validates the request again, runs the installed inocrium.exe
with --json, writes stdout, stderr (without progress records) and the exit code to
response.json, and exits. The calling copy writes inocrium's stdout (JSON) to its own stdout,
helper messages to stderr, deletes the folder and exits with inocrium's exit code.

Only `mount` and `unmount` are accepted. Passwords are never accepted as arguments. With
-Interactive the elevated window stays visible and, if the image needs a password, asks for it
with a hidden prompt; the password reaches inocrium.exe only through that child process's
INOCRIUM_PASSWORD environment variable and is never written anywhere.

.PARAMETER Arguments
The inocrium arguments without --json: `mount <image> [--disk N] [--partition N]
[--keep-permissions]` or `unmount <id|all>`. With powershell.exe -File, pass them separated by
spaces; inside PowerShell, -Arguments mount,'G:\x.irimg' also works.

.PARAMETER Interactive
Show the elevated window and prompt there for an image password when one is required.

.PARAMETER DryRun
Validate and write the request, print it with the elevated command line, then delete it.
Nothing is elevated or run.

.EXAMPLE
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Invoke-InocriumElevated.ps1 mount 'G:\backup-20261014-221351.irimg'

.EXAMPLE
& .\Invoke-InocriumElevated.ps1 -Arguments unmount,'0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0'

.NOTES
Exit codes: inocrium.exe's exit code (0 = success), or
  64   invalid helper arguments (nothing was run)
  65   the image needs a password: rerun with -Interactive (or the password was not accepted)
  66   inocrium.exe is not installed or the InocriumReflect service is not running
  67   inocrium.exe timed out and was stopped
  68   the elevated helper ended without a response (for example, its window was closed)
  1223 the UAC prompt was cancelled (nothing was run)
#>
[CmdletBinding(DefaultParameterSetName = 'Caller')]
param(
    [Parameter(ParameterSetName = 'Caller', Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Arguments,
    [Parameter(ParameterSetName = 'Caller')]
    [switch] $Interactive,
    [Parameter(ParameterSetName = 'Caller')]
    [ValidateRange(30, 3600)]
    [int] $TimeoutSeconds = 600,
    [Parameter(ParameterSetName = 'Caller')]
    [switch] $DryRun,
    [Parameter(ParameterSetName = 'Elevated', Mandatory = $true)]
    [string] $RequestPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:HelperPath = $PSCommandPath
$script:ServiceName = 'InocriumReflect'
$programFiles = $env:ProgramW6432
if ([string]::IsNullOrEmpty($programFiles)) { $programFiles = $env:ProgramFiles }
# Always the installed, admin-owned executable; the request file cannot choose another one.
$script:InocriumExe = Join-Path $programFiles 'inocrium reflect\inocrium.exe'
$script:Utf8 = New-Object Text.UTF8Encoding($false)

# Same as Get-QuotedNativeArgument in tests\Inocrium.TestHelpers.psm1, copied so that the skill
# works when installed outside the repository.
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

function Write-HelperMessage {
    param([string] $Message)
    [Console]::Error.WriteLine('inocrium-elevated: ' + $Message)
}

function New-UsageError {
    param([string] $Message)
    return New-Object ArgumentException $Message
}

function Resolve-ImagePath {
    param([string] $Path, [string] $BasePath)
    $Path = $Path.Replace('/', '\')
    if ($Path -match '^[A-Za-z]:(?!\\)') {
        throw (New-UsageError "Drive-relative paths are not supported: '$Path'. Use a full path such as G:\folder\image.irimg.")
    }
    if ($Path -match '^[A-Za-z]:\\' -or $Path.StartsWith('\\')) { $full = [IO.Path]::GetFullPath($Path) }
    elseif (-not [string]::IsNullOrEmpty($BasePath)) { $full = [IO.Path]::GetFullPath([IO.Path]::Combine($BasePath, $Path)) }
    else { throw (New-UsageError "The image path must be absolute: '$Path'.") }
    $extension = [IO.Path]::GetExtension($full).ToLowerInvariant()
    if ($extension -ne '.irimg' -and $extension -ne '.mrimgx') {
        throw (New-UsageError "Expected the first file of an image (*.irimg or *.mrimgx), not '$full'. Split parts (.i01, .i02, ...) are found automatically.")
    }
    if (-not [IO.File]::Exists($full)) {
        throw (New-UsageError "Image not found: '$full'. Mapped network drives are not visible to elevated processes or the service; use a local or UNC path.")
    }
    return $full
}

# Accepts only what the restore skill needs, so a tampered or mistaken request can never start a
# restore, backup or anything else. Returns normalized arguments (absolute image path, lower-case id).
function ConvertTo-ValidatedArguments {
    param([AllowNull()][object[]] $Arguments, [AllowNull()][string] $BasePath)
    $list = @($Arguments | Where-Object { $null -ne $_ } | ForEach-Object { [string]$_ })
    if ($list.Count -eq 0) {
        throw (New-UsageError 'No command given. Use: mount <image> [--disk N] [--partition N] [--keep-permissions], or unmount <id|all>.')
    }
    foreach ($value in $list) {
        if ($value -match '^(?i)-{1,2}password') {
            throw (New-UsageError 'Passwords are never passed as arguments. Run the helper with -Interactive and type the password into the elevated window.')
        }
    }
    $command = $list[0].ToLowerInvariant()
    $rest = @()
    if ($list.Count -gt 1) { $rest = @($list[1..($list.Count - 1)]) }

    if ($command -eq 'mount') {
        $image = $null
        $options = New-Object Collections.Generic.List[string]
        $seen = @{}
        for ($index = 0; $index -lt $rest.Count; $index++) {
            $token = $rest[$index]
            $name = $token.ToLowerInvariant()
            if ($name -eq '--keep-permissions' -or $name -eq '--disk' -or $name -eq '--partition') {
                if ($seen.ContainsKey($name)) { throw (New-UsageError "$name was given twice.") }
                $seen[$name] = $true
                $options.Add($name)
                if ($name -eq '--disk' -or $name -eq '--partition') {
                    $index++
                    if ($index -ge $rest.Count) { throw (New-UsageError "$name needs a number.") }
                    $minimum = 0
                    if ($name -eq '--partition') { $minimum = 1 }
                    if ($rest[$index] -notmatch '^\d{1,9}$' -or [int]$rest[$index] -lt $minimum) {
                        throw (New-UsageError "$name needs a number of at least $minimum, not '$($rest[$index])'.")
                    }
                    $options.Add(([int]$rest[$index]).ToString([Globalization.CultureInfo]::InvariantCulture))
                }
            }
            elseif ($name -eq '--writable') {
                throw (New-UsageError 'The restore skill mounts images read-only; --writable is not allowed.')
            }
            elseif ($token.StartsWith('-')) {
                throw (New-UsageError "Unsupported mount option '$token'. Allowed: --disk N, --partition N, --keep-permissions. Mounts are always read-only.")
            }
            elseif ($null -ne $image) {
                throw (New-UsageError "mount takes one image path; got '$image' and '$token'.")
            }
            else { $image = $token }
        }
        if ($null -eq $image) { throw (New-UsageError 'mount needs an image path.') }
        return [string[]](@('mount', (Resolve-ImagePath -Path $image -BasePath $BasePath)) + $options.ToArray())
    }
    if ($command -eq 'unmount') {
        if ($rest.Count -ne 1) { throw (New-UsageError 'unmount takes exactly one mount id or "all".') }
        if ($rest[0] -ieq 'all') { return [string[]]@('unmount', 'all') }
        $id = [guid]::Empty
        if (-not [guid]::TryParse($rest[0], [ref]$id)) { throw (New-UsageError "Not a mount id: '$($rest[0])'.") }
        return [string[]]@('unmount', $id.ToString('D'))
    }
    $hint = ''
    if ($list[0].Contains(',')) {
        $hint = ' With powershell.exe -File, separate the arguments with spaces (mount G:\x.irimg), not commas.'
    }
    throw (New-UsageError "Unsupported command '$($list[0])'. Only mount and unmount are allowed.$hint")
}

function Get-InocriumProblem {
    if (-not [IO.File]::Exists($script:InocriumExe)) {
        return "inocrium.exe was not found at '$($script:InocriumExe)'. Install the inocrium reflect MSI first."
    }
    $service = Get-Service -Name $script:ServiceName -ErrorAction SilentlyContinue
    if ($null -eq $service) { return "The $($script:ServiceName) service is not installed. Reinstall the inocrium reflect MSI." }
    if ($service.Status -ne 'Running') {
        # Without the service an elevated CLI falls back to a direct mount that lives only as long
        # as the hidden CLI process, so refuse instead.
        return "The $($script:ServiceName) service is $($service.Status). Start it (elevated: Start-Service $($script:ServiceName)) or reboot, then try again."
    }
    return $null
}

function Test-IsAdministrator {
    $principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-PasswordError {
    param([AllowEmptyString()][string] $Text)
    return $Text -match '(?i)password required|supplied password was rejected|password must not be empty'
}

function Remove-ProgressRecords {
    param([AllowEmptyString()][string] $Text)
    if ([string]::IsNullOrEmpty($Text)) { return '' }
    $lines = @($Text -split '\r?\n' | Where-Object { $_ -notmatch '^\s*\{\s*"type"\s*:\s*"progress"' })
    return ($lines -join [Environment]::NewLine).Trim()
}

function Invoke-InocriumProcess {
    param([string[]] $Arguments, [AllowNull()][Security.SecureString] $Password, [int] $TimeoutSeconds)
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $script:InocriumExe
    $startInfo.Arguments = (@('--json') + $Arguments | ForEach-Object { Get-QuotedNativeArgument -Value $_ }) -join ' '
    $startInfo.WorkingDirectory = Split-Path -Parent $script:InocriumExe
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.StandardOutputEncoding = $script:Utf8
    $startInfo.StandardErrorEncoding = $script:Utf8
    # Never let an inherited INOCRIUM_PASSWORD reach the CLI; only a password typed here may.
    $startInfo.EnvironmentVariables.Remove('INOCRIUM_PASSWORD')
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    $bstr = [IntPtr]::Zero
    try {
        if ($null -ne $Password) {
            $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($Password)
            $startInfo.EnvironmentVariables['INOCRIUM_PASSWORD'] = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
        }
        $null = $process.Start()
    }
    finally {
        # The child has its own environment block once started.
        $startInfo.EnvironmentVariables.Remove('INOCRIUM_PASSWORD')
        if ($bstr -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
    }
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.Close()
        $timedOut = -not $process.WaitForExit($TimeoutSeconds * 1000)
        if ($timedOut) {
            try { $process.Kill() } catch { }
        }
        $process.WaitForExit()
        $exitCode = 67
        if (-not $timedOut) { $exitCode = $process.ExitCode }
        return [pscustomobject]@{ ExitCode = $exitCode; Stdout = $stdout.Result; Stderr = $stderr.Result; TimedOut = $timedOut }
    }
    finally { $process.Dispose() }
}

function Invoke-ElevatedRequest {
    param([string] $Path)
    $response = [ordered]@{
        version = 1; exitCode = 1; stdout = ''; stderr = ''; error = $null; passwordRequired = $false; timedOut = $false
    }
    $responsePath = $null
    $interactive = $false
    try {
        $full = [IO.Path]::GetFullPath($Path)
        $folder = Split-Path -Parent $full
        if ((Split-Path -Leaf $full) -ne 'request.json' -or (Split-Path -Leaf $folder) -notmatch '^inocrium-elevated-[0-9a-f]{32}$') {
            throw "Unexpected request path '$full'."
        }
        $responsePath = Join-Path $folder 'response.json'
        $request = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($full, $script:Utf8))
        $interactive = [bool]$request.interactive
        if ($interactive) {
            try { $Host.UI.RawUI.WindowTitle = 'inocrium reflect (administrator)' } catch { }
        }
        $response.exitCode = 64
        $arguments = ConvertTo-ValidatedArguments -Arguments @($request.arguments) -BasePath $null
        $timeout = [Math]::Min([Math]::Max([int]$request.timeoutSeconds, 30), 3600)
        $response.exitCode = 1
        if (-not (Test-IsAdministrator)) { throw 'The helper is not running elevated.' }
        $response.exitCode = 66
        $problem = Get-InocriumProblem
        if ($null -ne $problem) { throw $problem }
        $response.exitCode = 1

        if ($interactive) { Write-Host ("Running: inocrium {0}" -f ($arguments -join ' ')) }
        $password = $null
        $attempt = 0
        while ($true) {
            $attempt++
            $result = Invoke-InocriumProcess -Arguments $arguments -Password $password -TimeoutSeconds $timeout
            if ($null -ne $password) { $password.Dispose(); $password = $null }
            $needsPassword = (-not $result.TimedOut) -and $result.ExitCode -ne 0 -and (Test-PasswordError -Text $result.Stderr)
            if (-not ($needsPassword -and $interactive -and $attempt -le 3)) { break }
            if ($attempt -gt 1) { Write-Host 'That password was not accepted.' -ForegroundColor Yellow }
            Write-Host 'This image is encrypted. Type its password here; it is not shown, saved or sent to the chat.'
            Write-Host 'Press Enter on an empty line to cancel.'
            $password = Read-Host -Prompt 'Image password' -AsSecureString
            if ($password.Length -eq 0) { $password.Dispose(); $password = $null; break }
        }
        $response.exitCode = $result.ExitCode
        $response.stdout = $result.Stdout
        $response.stderr = Remove-ProgressRecords -Text $result.Stderr
        $response.timedOut = $result.TimedOut
        $response.passwordRequired = $needsPassword
        if ($needsPassword) { $response.exitCode = 65 }
    }
    catch {
        $response.error = $_.Exception.Message
        if ($response.exitCode -eq 0) { $response.exitCode = 1 }
    }
    finally {
        if ($null -ne $responsePath) {
            [IO.File]::WriteAllText($responsePath, (ConvertTo-Json -InputObject $response -Depth 4), $script:Utf8)
        }
        if ($interactive) {
            if ($response.exitCode -eq 0) { Write-Host 'Done. You can go back to Claude Code.' -ForegroundColor Green }
            else { Write-Host 'Finished with an error; Claude Code will show the details.' -ForegroundColor Yellow }
            Start-Sleep -Seconds 3
        }
    }
    return [int]$response.exitCode
}

function Test-UacCancelled {
    param([Exception] $Exception)
    for ($current = $Exception; $null -ne $current; $current = $current.InnerException) {
        if ($current -is [ComponentModel.Win32Exception] -and $current.NativeErrorCode -eq 1223) { return $true }
    }
    return $false
}

function Invoke-Caller {
    $basePath = $null
    if ($PWD.Provider.Name -eq 'FileSystem') { $basePath = $PWD.ProviderPath }
    try { $validated = ConvertTo-ValidatedArguments -Arguments $Arguments -BasePath $basePath }
    catch {
        Write-HelperMessage $_.Exception.Message
        return 64
    }
    $problem = Get-InocriumProblem
    if ($null -ne $problem -and -not $DryRun) {
        Write-HelperMessage $problem
        return 66
    }

    $folder = Join-Path ([IO.Path]::GetTempPath()) ('inocrium-elevated-' + [guid]::NewGuid().ToString('N'))
    $null = [IO.Directory]::CreateDirectory($folder)
    try {
        $requestPath = Join-Path $folder 'request.json'
        $responsePath = Join-Path $folder 'response.json'
        $request = [ordered]@{
            version = 1; arguments = [string[]]$validated; interactive = [bool]$Interactive; timeoutSeconds = $TimeoutSeconds
        }
        [IO.File]::WriteAllText($requestPath, (ConvertTo-Json -InputObject $request -Depth 4), $script:Utf8)

        # Only fixed switches and paths this helper created go on the elevated command line.
        $hostExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $elevatedArguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass')
        if (-not $Interactive) { $elevatedArguments += '-NonInteractive' }
        $elevatedArguments += @('-File', $script:HelperPath, '-RequestPath', $requestPath)
        $commandLine = ($elevatedArguments | ForEach-Object { Get-QuotedNativeArgument -Value $_ }) -join ' '

        if ($DryRun) {
            $report = [ordered]@{
                dryRun = $true
                requestPath = $requestPath
                request = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($requestPath, $script:Utf8))
                elevatedCommand = (Get-QuotedNativeArgument -Value $hostExe) + ' ' + $commandLine
                inocriumProblem = $problem
            }
            $script:Output.Add((ConvertTo-Json -InputObject $report -Depth 5 -Compress))
            return 0
        }

        # Process.Start with the runas verb is what Start-Process -Verb RunAs does, but it keeps the
        # Win32Exception, so a cancelled UAC prompt (1223) is recognized on any display language.
        $startInfo = New-Object Diagnostics.ProcessStartInfo
        $startInfo.FileName = $hostExe
        $startInfo.Arguments = $commandLine
        $startInfo.UseShellExecute = $true
        $startInfo.Verb = 'runas'
        if ($Interactive) { $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Normal }
        else { $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden }
        try { $process = [Diagnostics.Process]::Start($startInfo) }
        catch {
            if (Test-UacCancelled -Exception $_.Exception) {
                Write-HelperMessage 'The administrator (UAC) prompt was cancelled or declined; nothing was run.'
                return 1223
            }
            throw
        }
        try { $process.WaitForExit() } finally { $process.Dispose() }

        if (-not [IO.File]::Exists($responsePath)) {
            Write-HelperMessage 'The elevated helper ended without a response (was its window closed?). Nothing is known about the result; check the Mounts page in the inocrium GUI.'
            return 68
        }
        $response = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($responsePath, $script:Utf8))
        if (-not [string]::IsNullOrWhiteSpace($response.stdout)) {
            foreach ($line in ($response.stdout.Trim() -split '\r?\n')) { $script:Output.Add($line) }
        }
        if (-not [string]::IsNullOrWhiteSpace($response.stderr)) { [Console]::Error.WriteLine($response.stderr) }
        if ($null -ne $response.error) { Write-HelperMessage $response.error }
        if ($response.passwordRequired) {
            if ($Interactive) { Write-HelperMessage 'The image password was not provided or not accepted.' }
            else { Write-HelperMessage 'This image needs a password. Run the helper again with -Interactive; the user types the password into the elevated window, never into the chat.' }
        }
        if ($response.timedOut) {
            Write-HelperMessage "inocrium.exe did not finish within $TimeoutSeconds seconds and was stopped. The service may still finish the operation; check the Mounts page in the inocrium GUI."
        }
        return [int]$response.exitCode
    }
    finally {
        Remove-Item -LiteralPath $folder -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($PSCmdlet.ParameterSetName -eq 'Elevated') { exit (Invoke-ElevatedRequest -Path $RequestPath) }
# Functions return only exit codes; inocrium's stdout is collected here and becomes this
# script's output, so callers can capture or parse it.
$script:Output = New-Object Collections.Generic.List[string]
$code = Invoke-Caller
foreach ($line in $script:Output) { Write-Output $line }
exit $code
