# Written by yanuget-internalize (INT-19). This package's downloads were
# internalized: each file it asks for is attached to it in a YANuget feed, and
# this prelude sends Chocolatey's download helpers there instead of to the
# internet. internalizer/report.json in the package lists the original URLs.
#
# A URL that is not in the map below stops the install; nothing falls back
# to the internet. Files are checked against their SHA-256, whatever checksum
# the package declares.
#
# The feed the files come from. Set YANUGET_INTERNALIZED_FEED_URL on the
# client to fetch them from another address, after the feed moved hosts.
$YanugetFeedUrl = '@FEED_URL@'

function __Yanuget-FeedUrl {
    $url = $YanugetFeedUrl
    if ($env:YANUGET_INTERNALIZED_FEED_URL) { $url = $env:YANUGET_INTERNALIZED_FEED_URL }
    if (-not $url.EndsWith('/')) { $url += '/' }
    $url
}

# The attached files, keyed by the SHA-256 of the URL the package asked for
# (UTF-8, lower-case hex): the file's path under the feed, and its SHA-256.
function __Yanuget-Files {
    @{
@FILES@
    }
}

function __Yanuget-Key([string] $Text) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($Text))
        -join ($hash | ForEach-Object { $_.ToString('x2') })
    } finally {
        $sha.Dispose()
    }
}

# The attached file for a URL: as the package spelled it, else in the
# canonical form the capture recorded it in (lower-case scheme and host, no
# default port, escaped).
function __Yanuget-Find([string] $Url) {
    $files = __Yanuget-Files
    $file = $files[(__Yanuget-Key $Url)]
    if ($null -ne $file) { return , $file }
    $uri = $null
    if ([System.Uri]::TryCreate($Url, [System.UriKind]::Absolute, [ref] $uri)) {
        $file = $files[(__Yanuget-Key $uri.AbsoluteUri)]
    }
    , $file
}

# For each helper: its URL parameters, each with the checksum parameters that
# pin it. A helper without checksum parameters is checked once it is done.
function __Yanuget-Spec([string] $Helper) {
    switch ($Helper) {
        'Get-WebFile' { , @(, @('Url', $null, $null)) }
        'Get-FtpFile' { , @(, @('Url', $null, $null)) }
        'Install-ChocolateyVsixPackage' { , @(, @('VsixUrl', 'Checksum', 'ChecksumType')) }
        default {
            , @(
                @('Url', 'Checksum', 'ChecksumType'),
                @('Url64bit', 'Checksum64', 'ChecksumType64')
            )
        }
    }
}

# Point the helper's URL parameters at the attached files, with their
# SHA-256 as checksum. Returns the SHA-256 a helper without checksum
# parameters has to be checked against afterwards.
function __Yanuget-Redirect($Bound, [string] $Helper) {
    $expect = $null
    foreach ($s in (__Yanuget-Spec $Helper)) {
        if (-not $Bound.ContainsKey($s[0])) { continue }
        $url = [string] $Bound[$s[0]]
        # A local path is no download.
        if ($url -notmatch '^(?i)(https?|ftps?)://') { continue }
        $file = __Yanuget-Find $url
        if ($null -eq $file) {
            throw ("yanuget: the internalized package $env:ChocolateyPackageName " +
                "$env:ChocolateyPackageVersion asks $Helper for $url, which it does not carry. " +
                'Internalize it again, or add the download to its override.')
        }
        $Bound[$s[0]] = (__Yanuget-FeedUrl) + $file[0]
        if ($null -ne $s[1]) {
            $Bound[$s[1]] = $file[1]
            $Bound[$s[2]] = 'sha256'
        } else {
            $expect = $file[1]
        }
    }
    $expect
}

# What Get-WebFile wrote, checked against the attached file's SHA-256.
function __Yanuget-Verify($Bound, [string] $Helper, $Expect) {
    if ($null -eq $Expect) { return }
    $path = $null
    if ($Bound.ContainsKey('FileName')) { $path = [string] $Bound['FileName'] }
    if (-not $path -or -not (Test-Path -LiteralPath $path)) {
        throw "yanuget: $Helper downloaded a file the prelude cannot check."
    }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expect) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
        throw "yanuget: $path does not have the SHA-256 of the attached file."
    }
}

# Each helper is wrapped by a proxy command made from the installed helper
# itself, so positional parameters and aliases bind as they do against it.
# The wrapper calls the module's own helper: calls from inside the module go
# to the internet as before, which the capture made sure none of this
# package's do.
foreach ($__yanugetHelper in @(
        'Get-ChocolateyWebFile', 'Get-WebFile', 'Get-FtpFile',
        'Install-ChocolateyPackage', 'Install-ChocolateyZipPackage',
        'Install-ChocolateyVsixPackage', 'Install-ChocolateyPowershellCommand')) {
    $__yanugetCmd = Get-Command -Name $__yanugetHelper -Module chocolateyInstaller `
        -CommandType Function -ErrorAction SilentlyContinue
    if ($null -eq $__yanugetCmd) { continue }
    $__yanugetBody = [System.Management.Automation.ProxyCommand]::Create(
        (New-Object System.Management.Automation.CommandMetadata $__yanugetCmd))
    $__yanugetPlain = "GetCommand('$__yanugetHelper',"
    $__yanugetQualified = "GetCommand('chocolateyInstaller\$__yanugetHelper',"
    if ($__yanugetBody.Contains($__yanugetPlain)) {
        $__yanugetBody = $__yanugetBody.Replace($__yanugetPlain, $__yanugetQualified)
    }
    $__yanugetStart = '$wrappedCmd = $ExecutionContext.InvokeCommand.' + $__yanugetQualified
    $__yanugetEnd = '$steppablePipeline.End()'
    $__yanugetAt = $__yanugetBody.IndexOf($__yanugetStart)
    if ($__yanugetAt -lt 0 -or
        $__yanugetBody.IndexOf($__yanugetStart, $__yanugetAt + 1) -ge 0 -or
        $__yanugetBody.IndexOf($__yanugetEnd) -lt 0 -or
        $__yanugetBody.IndexOf($__yanugetEnd, $__yanugetBody.IndexOf($__yanugetEnd) + 1) -ge 0) {
        throw "yanuget: cannot wrap $__yanugetHelper of this Chocolatey version."
    }
    $__yanugetBody = $__yanugetBody.Insert($__yanugetAt,
        "`$__yanugetExpect = __Yanuget-Redirect `$PSBoundParameters '$__yanugetHelper'`n        ")
    $__yanugetBody = $__yanugetBody.Replace($__yanugetEnd,
        "$__yanugetEnd`n        __Yanuget-Verify `$PSBoundParameters '$__yanugetHelper' `$__yanugetExpect")
    . ([scriptblock]::Create("function $__yanugetHelper {`n$__yanugetBody`n}"))
}
Remove-Variable -Name __yanugetHelper, __yanugetCmd, __yanugetBody, __yanugetPlain, `
    __yanugetQualified, __yanugetStart, __yanugetEnd, __yanugetAt -ErrorAction SilentlyContinue
