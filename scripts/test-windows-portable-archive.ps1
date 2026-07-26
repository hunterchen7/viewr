[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ArchivePath,

    [Parameter(Mandatory = $true)]
    [string] $ExpectedBinaryPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$ArchivePath = [IO.Path]::GetFullPath($ArchivePath)
$ExpectedBinaryPath = [IO.Path]::GetFullPath($ExpectedBinaryPath)

if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
    throw "Portable archive does not exist: $ArchivePath"
}
if (-not (Test-Path -LiteralPath $ExpectedBinaryPath -PathType Leaf)) {
    throw "Expected Viewr binary does not exist: $ExpectedBinaryPath"
}

$temporaryDirectory = Join-Path (
    [IO.Path]::GetTempPath()
) ("viewr-portable-test-" + [Guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$corruptArchive = Join-Path $temporaryDirectory "viewr-windows-x64.zip"

try {
    Copy-Item -LiteralPath $ArchivePath -Destination $corruptArchive
    [byte[]] $archiveBytes = [IO.File]::ReadAllBytes($corruptArchive)

    $minimumEocdLength = 22
    $maximumCommentLength = 65535
    $minimumEocdOffset = [Math]::Max(
        0,
        $archiveBytes.Length - $minimumEocdLength - $maximumCommentLength
    )
    $eocdOffset = -1
    for (
        $index = $archiveBytes.Length - $minimumEocdLength;
        $index -ge $minimumEocdOffset;
        $index--
    ) {
        if (
            $archiveBytes[$index] -eq 0x50 -and
            $archiveBytes[$index + 1] -eq 0x4b -and
            $archiveBytes[$index + 2] -eq 0x05 -and
            $archiveBytes[$index + 3] -eq 0x06
        ) {
            $eocdOffset = $index
            break
        }
    }
    if ($eocdOffset -lt 0) {
        throw "Could not find the ZIP end-of-central-directory record."
    }

    $centralDirectoryOffset = [int64] [BitConverter]::ToUInt32(
        $archiveBytes,
        $eocdOffset + 16
    )
    if (
        $centralDirectoryOffset -lt 0 -or
        $centralDirectoryOffset + 46 -gt $archiveBytes.Length -or
        $archiveBytes[$centralDirectoryOffset] -ne 0x50 -or
        $archiveBytes[$centralDirectoryOffset + 1] -ne 0x4b -or
        $archiveBytes[$centralDirectoryOffset + 2] -ne 0x01 -or
        $archiveBytes[$centralDirectoryOffset + 3] -ne 0x02
    ) {
        throw "The ZIP central directory is malformed."
    }

    $crcOffset = $centralDirectoryOffset + 16
    $archiveBytes[$crcOffset] = $archiveBytes[$crcOffset] -bxor 0xff
    [IO.File]::WriteAllBytes($corruptArchive, $archiveBytes)

    $rejected = $false
    try {
        & "$PSScriptRoot/validate-portable-archive.ps1" `
            -Platform windows-x64 `
            -ArchivePath $corruptArchive `
            -ExpectedBinaryPath $ExpectedBinaryPath
    }
    catch {
        $rejected = $true
    }
    finally {
        $global:LASTEXITCODE = 0
    }

    if (-not $rejected) {
        throw "The validator accepted a ZIP with a corrupt stored CRC-32."
    }
}
finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}

Write-Host "Portable archive corruption tests passed."
