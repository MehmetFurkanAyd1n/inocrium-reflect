<#
.SYNOPSIS
Converts a Chromium bookmarks file (Bookmarks or AccountBookmarks) into an HTML file that any
browser can import.

.DESCRIPTION
Chrome keeps the bookmarks of a signed-in Google account in AccountBookmarks, apart from the local
Bookmarks file. Copying AccountBookmarks into a profile that is not signed in to the same account
does not bring them back reliably, so the restore skill converts it and the user imports the HTML
(chrome://bookmarks -> menu -> Import bookmarks). Only reads -Path; writes only -Destination.

.EXAMPLE
.\ConvertTo-BookmarksHtml.ps1 -Path 'E:\Users\inosh\AppData\Local\Google\Chrome\User Data\Default\AccountBookmarks' -Destination "$env:USERPROFILE\Desktop\chrome-account-bookmarks.html"
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Path,
    [Parameter(Mandatory)] [string] $Destination
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (Test-Path -LiteralPath $Destination) { throw "$Destination already exists; choose another name." }
$json = [IO.File]::ReadAllText($Path, [Text.Encoding]::UTF8) | ConvertFrom-Json
$out = New-Object Text.StringBuilder
$script:count = 0

# Chromium times are microseconds since 1601; the HTML format uses Unix seconds.
function Get-UnixTime([object] $Node) {
    $value = 0L
    if ($Node.PSObject.Properties['date_added'] -and [long]::TryParse([string]$Node.date_added, [ref]$value) -and $value -gt 0) {
        return [long]($value / 1000000 - 11644473600)
    }
    return 0
}

function Add-Node([object] $Node, [string] $Indent) {
    $name = [Net.WebUtility]::HtmlEncode([string]$Node.name)
    if ($Node.type -eq 'url') {
        $url = [Net.WebUtility]::HtmlEncode([string]$Node.url)
        $null = $out.AppendLine("$Indent<DT><A HREF=`"$url`" ADD_DATE=`"$(Get-UnixTime $Node)`">$name</A>")
        $script:count++
        return
    }
    $null = $out.AppendLine("$Indent<DT><H3 ADD_DATE=`"$(Get-UnixTime $Node)`">$name</H3>")
    Add-Folder $Node $Indent
}

function Add-Folder([object] $Folder, [string] $Indent) {
    $null = $out.AppendLine("$Indent<DL><p>")
    if ($Folder.PSObject.Properties['children']) {
        foreach ($child in @($Folder.children)) { Add-Node $child ($Indent + '    ') }
    }
    $null = $out.AppendLine("$Indent</DL><p>")
}

$null = $out.AppendLine('<!DOCTYPE NETSCAPE-Bookmark-file-1>')
$null = $out.AppendLine('<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">')
$null = $out.AppendLine('<TITLE>Bookmarks</TITLE>')
$null = $out.AppendLine('<H1>Bookmarks</H1>')
$null = $out.AppendLine('<DL><p>')
foreach ($root in 'bookmark_bar', 'other', 'synced') {
    if (-not $json.roots.PSObject.Properties[$root]) { continue }
    $node = $json.roots.$root
    if (-not $node.PSObject.Properties['children'] -or @($node.children).Count -eq 0) { continue }
    $toolbar = if ($root -eq 'bookmark_bar') { ' PERSONAL_TOOLBAR_FOLDER="true"' } else { '' }
    $null = $out.AppendLine("    <DT><H3 ADD_DATE=`"$(Get-UnixTime $node)`"$toolbar>$([Net.WebUtility]::HtmlEncode([string]$node.name))</H3>")
    Add-Folder $node '    '
}
$null = $out.AppendLine('</DL><p>')
[IO.File]::WriteAllText($Destination, $out.ToString(), (New-Object Text.UTF8Encoding($false)))
[pscustomobject]@{ Destination = $Destination; Bookmarks = $script:count }
