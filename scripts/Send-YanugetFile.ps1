# Upload a file to a package version, resumably (tus 1.0.0). PowerShell 7+.
# Re-run with -Resume <url> after an interruption to continue where it stopped.
#
# The push key goes out on every request, so the function is strict about where
# it sends it:
#   - the feed must be https; -AllowHttp is for a local test server only;
#   - the upload URL the server hands back (and any -Resume URL) must be on the
#     same scheme, host and port as -Feed, so a hostile or misconfigured
#     `Location` cannot collect the key;
#   - redirects are not followed, for the same reason.
# The key is read from -ApiKey as a SecureString, else from the
# YANUGET_API_KEY environment variable, else prompted for — never as a plain
# string on the command line, where it would end up in the shell history.
function Send-YanugetFile {
    param(
        [Parameter(Mandatory)] [string] $Feed,      # e.g. https://nuget.example.com
        [Parameter(Mandatory)] [string] $Id,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $Path,
        [SecureString] $ApiKey,
        [string] $Resume,
        [long] $ChunkSize = 64MB,
        [switch] $AllowHttp
    )
    $base = $Feed.TrimEnd('/')
    $feedUri = [Uri]$base
    if ($feedUri.Scheme -ne 'https' -and -not ($AllowHttp -and $feedUri.Scheme -eq 'http')) {
        throw "Refusing to send the API key to $Feed over $($feedUri.Scheme); use https (or -AllowHttp for a local test server)."
    }
    # Only accept an upload URL on the origin the key was meant for.
    $sameOrigin = {
        param([Uri] $u)
        $u.IsAbsoluteUri -and $u.Scheme -eq $feedUri.Scheme -and
            $u.Host -eq $feedUri.Host -and $u.Port -eq $feedUri.Port
    }

    $key = if ($ApiKey) {
        [Net.NetworkCredential]::new('', $ApiKey).Password
    } elseif ($env:YANUGET_API_KEY) {
        $env:YANUGET_API_KEY
    } else {
        [Net.NetworkCredential]::new('', (Read-Host -AsSecureString 'API key')).Password
    }
    $headers = @{ 'Tus-Resumable' = '1.0.0'; 'X-NuGet-ApiKey' = $key }
    $web = @{ MaximumRedirection = 0 }
    $b64 = { param($s) [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($s)) }
    $length = (Get-Item $Path).Length
    $url = $Resume
    if (-not $url) {
        $sha = (Get-FileHash $Path -Algorithm SHA256).Hash.ToLower()
        $meta = @(
            "id $(& $b64 $Id)", "version $(& $b64 $Version)",
            "filename $(& $b64 (Split-Path $Path -Leaf))", "sha256 $(& $b64 $sha)"
        ) -join ','
        $created = Invoke-WebRequest @web -Method Post -Uri "$base/api/v2/uploads" -Headers ($headers + @{
            'Upload-Length' = "$length"; 'Upload-Metadata' = $meta })
        # A relative Location is resolved against the feed, as a browser would.
        $url = [string][Uri]::new($feedUri, [string]$created.Headers.Location)
        Write-Host "Upload started; to resume after an interruption: -Resume $url"
    }
    if (-not (& $sameOrigin ([Uri]$url))) {
        throw "Refusing to send the API key to $url, which is not on $($feedUri.GetLeftPart('Authority'))."
    }
    # Where to go on from: the server knows what arrived.
    $offset = [long][string](Invoke-WebRequest @web -Method Head -Uri $url -Headers $headers).Headers['Upload-Offset']
    $file = [IO.File]::OpenRead($Path)
    try {
        while ($offset -lt $length) {
            $buffer = [byte[]]::new([Math]::Min($ChunkSize, $length - $offset))
            $file.Position = $offset
            $read = 0
            while ($read -lt $buffer.Length) { $read += $file.Read($buffer, $read, $buffer.Length - $read) }
            $sent = Invoke-WebRequest @web -Method Patch -Uri $url -Body $buffer -ContentType 'application/offset+octet-stream' `
                -Headers ($headers + @{ 'Upload-Offset' = "$offset" })
            $offset = [long][string]$sent.Headers['Upload-Offset']
            Write-Progress -Activity "Uploading $(Split-Path $Path -Leaf)" -PercentComplete (100 * $offset / $length)
        }
    } finally { $file.Dispose() }
}
