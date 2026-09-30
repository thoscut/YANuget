# Upload a file to a package version, resumably (tus 1.0.0). PowerShell 7+.
# Re-run with -Resume <url> after an interruption to continue where it stopped.
function Send-YanugetFile {
    param(
        [Parameter(Mandatory)] [string] $Feed,      # e.g. https://nuget.example.com
        [Parameter(Mandatory)] [string] $ApiKey,
        [Parameter(Mandatory)] [string] $Id,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $Path,
        [string] $Resume,
        [long] $ChunkSize = 64MB
    )
    $headers = @{ 'Tus-Resumable' = '1.0.0'; 'X-NuGet-ApiKey' = $ApiKey }
    $b64 = { param($s) [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($s)) }
    $length = (Get-Item $Path).Length
    $url = $Resume
    if (-not $url) {
        $sha = (Get-FileHash $Path -Algorithm SHA256).Hash.ToLower()
        $meta = @(
            "id $(& $b64 $Id)", "version $(& $b64 $Version)",
            "filename $(& $b64 (Split-Path $Path -Leaf))", "sha256 $(& $b64 $sha)"
        ) -join ','
        $created = Invoke-WebRequest -Method Post -Uri "$Feed/api/v2/uploads" -Headers ($headers + @{
            'Upload-Length' = "$length"; 'Upload-Metadata' = $meta })
        $url = [string]$created.Headers.Location
        Write-Host "Upload started; to resume after an interruption: -Resume $url"
    }
    # Where to go on from: the server knows what arrived.
    $offset = [long][string](Invoke-WebRequest -Method Head -Uri $url -Headers $headers).Headers['Upload-Offset']
    $file = [IO.File]::OpenRead($Path)
    try {
        while ($offset -lt $length) {
            $buffer = [byte[]]::new([Math]::Min($ChunkSize, $length - $offset))
            $file.Position = $offset
            $read = 0
            while ($read -lt $buffer.Length) { $read += $file.Read($buffer, $read, $buffer.Length - $read) }
            $sent = Invoke-WebRequest -Method Patch -Uri $url -Body $buffer -ContentType 'application/offset+octet-stream' `
                -Headers ($headers + @{ 'Upload-Offset' = "$offset" })
            $offset = [long][string]$sent.Headers['Upload-Offset']
            Write-Progress -Activity "Uploading $(Split-Path $Path -Leaf)" -PercentComplete (100 * $offset / $length)
        }
    } finally { $file.Dispose() }
}
