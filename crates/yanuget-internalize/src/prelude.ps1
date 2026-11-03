# Written by yanuget-internalize (INT-19). This package's downloads were
# internalized: each file it asks for is attached to it in a YANuget feed, and
# this prelude sends Chocolatey's download helpers there instead of to the
# internet. internalizer/report.json in the package lists the original URLs.
#
# A URL that is not in the map below stops the install; nothing falls back
# to the internet, and a path to another machine (\\host\share, a network
# drive) is refused as well. Files are checked against their SHA-256,
# whatever checksum the package declares, and --ignore-checksums does not
# turn that off. The package's uninstall and before-modify scripts load this
# prelude too.
#
# The feed the files come from. Set YANUGET_INTERNALIZED_FEED_URL on the
# client to fetch them from another address, after the feed moved hosts. It
# has to be an http or https URL; plain http is taken, with a warning, since
# the SHA-256 is what vouches for each file.
$YanugetFeedUrl = '@FEED_URL@'

function __Yanuget-FeedUrl {
    $url = $YanugetFeedUrl
    $moved = $env:YANUGET_INTERNALIZED_FEED_URL
    if ($moved) {
        $url = $moved.Trim()
        $uri = $null
        if (-not [System.Uri]::TryCreate($url, [System.UriKind]::Absolute, [ref] $uri) -or
            ($uri.Scheme -ne 'http' -and $uri.Scheme -ne 'https') -or -not $uri.Host) {
            throw ("yanuget: YANUGET_INTERNALIZED_FEED_URL is $moved, which is not the " +
                'http or https URL of a feed.')
        }
        if ($uri.Scheme -eq 'http') {
            Write-Warning ('yanuget: YANUGET_INTERNALIZED_FEED_URL is a plain http URL; ' +
                'each file is still checked against its SHA-256.')
        }
    }
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

# The attached file for a URL: as the package spelled it, without the blanks
# around it, else in the canonical form the capture recorded it in
# (lower-case scheme and host, no default port, '/' for '\', escaped), which
# is also what .NET makes of it when it downloads.
function __Yanuget-Find([string] $Url) {
    $files = __Yanuget-Files
    foreach ($spelling in @($Url, $Url.Trim())) {
        $file = $files[(__Yanuget-Key $spelling)]
        if ($null -ne $file) { return , $file }
    }
    $uri = $null
    if ([System.Uri]::TryCreate($Url.Trim(), [System.UriKind]::Absolute, [ref] $uri)) {
        $file = $files[(__Yanuget-Key $uri.AbsoluteUri)]
    }
    , $file
}

# Whether the drive letter $Name is on another machine: a mapped network
# drive, or a PowerShell drive rooted at a UNC path. A drive this machine
# does not have names no file, and is left to the helper to fail on.
function __Yanuget-IsNetworkDrive([string] $Name) {
    $drive = Get-PSDrive -Name $Name -PSProvider FileSystem -ErrorAction SilentlyContinue
    if ($null -ne $drive) {
        foreach ($root in @($drive.Root, $drive.DisplayRoot)) {
            if ($root -and ([string] $root) -match '^[\\/]{2}') { return $true }
        }
    }
    # Drive letters are Windows'; elsewhere there is no DriveInfo to ask.
    if ([System.IO.Path]::DirectorySeparatorChar -ne '\') { return $false }
    try {
        (New-Object System.IO.DriveInfo -ArgumentList $Name).DriveType -eq
            [System.IO.DriveType]::Network
    } catch {
        $true
    }
}

# Whether a relative path stays on this machine: the folder it is relative
# to, PowerShell's and the process's, is not on another one.
function __Yanuget-IsLocalHere {
    $fs = Get-Location -PSProvider FileSystem -ErrorAction SilentlyContinue
    $here = @([System.IO.Directory]::GetCurrentDirectory())
    if ($null -ne $fs) { $here += $fs.ProviderPath }
    foreach ($dir in $here) {
        if ($dir -match '^[\\/]{2}') { return $false }
        if ($dir -match '^([a-z]):' -and (__Yanuget-IsNetworkDrive $Matches[1])) { return $false }
    }
    $true
}

# Whether a URL parameter's value names a file on this machine, which the
# helpers copy rather than download: a drive path, a path relative to the
# current folder or drive, or a file: URI without a host, each also with the
# FileSystem provider's prefix (FileSystem::, Microsoft.PowerShell.Core\
# FileSystem::), which PowerShell takes off. A UNC path (\\host\share,
# //host/share, \\?\ and \\.\ alike), a network drive, a file: URI with a host,
# another provider and any other scheme would reach the network another
# way, or are no file, and are not.
function __Yanuget-IsLocal([string] $Path) {
    $at = $Path.IndexOf('::')
    if ($at -ge 0) {
        if ($Path.Substring(0, $at) -notmatch '^(Microsoft\.PowerShell\.Core\\)?FileSystem$') {
            return $false
        }
        $Path = $Path.Substring($at + 2)
        if ($Path.Contains('::')) { return $false }
    }
    if (-not $Path) { return $false }
    if ($Path -match '^[\\/]{2}') { return $false }
    if ($Path -match '^([a-z]):') { return (-not (__Yanuget-IsNetworkDrive $Matches[1])) }
    if ($Path -match '^file:') {
        $uri = $null
        if (-not ([System.Uri]::TryCreate($Path, [System.UriKind]::Absolute, [ref] $uri) -and
                $uri.IsFile -and -not $uri.IsUnc -and $uri.LocalPath -notmatch '^[\\/]{2}')) {
            return $false
        }
        return (__Yanuget-IsLocal $uri.LocalPath)
    }
    if ($Path -match '^[a-z][a-z0-9+.-]*:') { return $false }
    __Yanuget-IsLocalHere
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
# SHA-256 as checksum. Returns the SHA-256 of each file redirected to, for
# __Yanuget-Enforce and __Yanuget-Verify.
function __Yanuget-Redirect($Bound, [string] $Helper) {
    $expect = @()
    foreach ($s in (__Yanuget-Spec $Helper)) {
        if (-not $Bound.ContainsKey($s[0])) { continue }
        $url = [string] $Bound[$s[0]]
        $probe = $url.Trim()
        if (-not $probe) { continue }
        # The helpers download what starts with http or ftp, and .NET forgives
        # the blanks, case and backslashes the package may have written
        # (' https:\\host\x'): all of it is looked up in the map. Anything
        # else is copied, which is fine for a file on this machine only.
        if ($probe -notmatch '^(http|ftp)') {
            if (__Yanuget-IsLocal $probe) { continue }
            throw ("yanuget: the internalized package $env:ChocolateyPackageName " +
                "$env:ChocolateyPackageVersion asks $Helper for $url, which is neither a " +
                'download it carries nor a file on this machine. ' +
                'Internalize it again, or add the download to its override.')
        }
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
        }
        $expect += $file[1]
    }
    , $expect
}

# Chocolatey skips the checksum it was given under --ignore-checksums (or
# with the checksumFiles feature off), which it tells its helpers through
# $env:ChocolateyIgnoreChecksums. The helpers that install what they
# downloaded do so before they return, so the pin has to hold inside them:
# while a helper fetches an attached file the variable is taken away.
# Returns its value, for __Yanuget-Verify to put back.
function __Yanuget-Enforce($Expect) {
    if ($Expect.Count -eq 0 -or $env:ChocolateyIgnoreChecksums -ne 'true') { return $null }
    Write-Warning ('yanuget: the files of this internalized package are checked against ' +
        'their SHA-256, --ignore-checksums notwithstanding.')
    Remove-Item -Path Env:ChocolateyIgnoreChecksums -ErrorAction SilentlyContinue
    'true'
}

# Once the helper is done: $env:ChocolateyIgnoreChecksums put back, and what
# a helper that only downloads wrote checked against the attached file's
# SHA-256, whatever Chocolatey checked. Get-WebFile and Get-FtpFile take no
# checksum, so theirs must be there to be checked; the Install-* helpers ran
# theirs already, under the checksum enforced above.
function __Yanuget-Verify($Bound, [string] $Helper, $Expect, $Ignored) {
    if ($null -ne $Ignored) { $env:ChocolateyIgnoreChecksums = $Ignored }
    if ($null -eq $Expect -or $Expect.Count -eq 0) { return }
    $path = $null
    $must = $false
    if ($Helper -eq 'Get-WebFile' -or $Helper -eq 'Get-FtpFile') {
        $must = $true
        if ($Bound.ContainsKey('FileName')) { $path = [string] $Bound['FileName'] }
    } elseif ($Helper -eq 'Get-ChocolateyWebFile') {
        # Under -GetOriginalFileName the file is named for the server's
        # answer, not FileFullPath; Chocolatey's check, enforced above,
        # covers it.
        $original = $Bound.ContainsKey('GetOriginalFileName') -and $Bound['GetOriginalFileName']
        if ($Bound.ContainsKey('FileFullPath') -and -not $original) {
            $path = [string] $Bound['FileFullPath']
        }
    } else {
        return
    }
    if (-not $path -or -not (Test-Path -LiteralPath $path)) {
        if ($must) { throw "yanuget: $Helper downloaded a file the prelude cannot check." }
        return
    }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Expect -notcontains $actual) {
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
        "`$__yanugetExpect = __Yanuget-Redirect `$PSBoundParameters '$__yanugetHelper'`n        " +
        "`$__yanugetIgnored = __Yanuget-Enforce `$__yanugetExpect`n        ")
    $__yanugetBody = $__yanugetBody.Replace($__yanugetEnd,
        "$__yanugetEnd`n        __Yanuget-Verify `$PSBoundParameters '$__yanugetHelper' " +
        "`$__yanugetExpect `$__yanugetIgnored")
    . ([scriptblock]::Create("function $__yanugetHelper {`n$__yanugetBody`n}"))
}
Remove-Variable -Name __yanugetHelper, __yanugetCmd, __yanugetBody, __yanugetPlain, `
    __yanugetQualified, __yanugetStart, __yanugetEnd, __yanugetAt -ErrorAction SilentlyContinue
