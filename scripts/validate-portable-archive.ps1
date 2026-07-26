[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("windows-x64")]
    [string] $Platform,

    [Parameter(Mandatory = $true)]
    [string] $ArchivePath,

    [Parameter(Mandatory = $true)]
    [string] $ExpectedBinaryPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-X64Pe {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 0x40 -or $reader.ReadUInt16() -ne 0x5a4d) {
            throw "'$Path' is not a PE executable (missing MZ signature)."
        }

        $stream.Position = 0x3c
        $peOffset = [int64] $reader.ReadUInt32()
        if ($peOffset -gt $stream.Length - 26) {
            throw "'$Path' has a truncated PE header."
        }

        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "'$Path' is not a PE executable (missing PE signature)."
        }
        if ($reader.ReadUInt16() -ne 0x8664) {
            throw "'$Path' is not an x64 PE executable."
        }

        $stream.Position = $peOffset + 22
        $characteristics = $reader.ReadUInt16()
        if (($characteristics -band 0x0002) -eq 0) {
            throw "'$Path' is not marked as executable."
        }
        if (($characteristics -band 0x2000) -ne 0) {
            throw "'$Path' is a DLL, not an executable."
        }

        $stream.Position = $peOffset + 24
        if ($reader.ReadUInt16() -ne 0x020b) {
            throw "'$Path' is not a PE32+ executable."
        }
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Assert-SameFileHash {
    param(
        [Parameter(Mandatory = $true)]
        [string] $ActualPath,

        [Parameter(Mandatory = $true)]
        [string] $ExpectedPath,

        [Parameter(Mandatory = $true)]
        [string] $Description
    )

    $actualHash = (
        Get-FileHash -LiteralPath $ActualPath -Algorithm SHA256
    ).Hash
    $expectedHash = (
        Get-FileHash -LiteralPath $ExpectedPath -Algorithm SHA256
    ).Hash
    if ($actualHash -cne $expectedHash) {
        throw "$Description differs from its expected source."
    }
}

function Assert-ZipIntegrity {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $sevenZip = Get-Command `
        "7z.exe" `
        -CommandType Application `
        -ErrorAction SilentlyContinue
    if ($null -eq $sevenZip) {
        $sevenZip = Get-Command `
            "7z" `
            -CommandType Application `
            -ErrorAction SilentlyContinue
    }
    if ($null -eq $sevenZip) {
        throw "7-Zip is required to validate ZIP integrity."
    }

    $integrityOutput = & $sevenZip.Source "t" "-bd" "-bb0" $Path 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw (
            "7-Zip rejected the portable archive with exit code " +
            "$LASTEXITCODE`n$($integrityOutput | Out-String)"
        )
    }
}

if ($Platform -ne "windows-x64") {
    throw "Unsupported platform '$Platform'."
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$ArchivePath = [IO.Path]::GetFullPath($ArchivePath)
$ExpectedBinaryPath = [IO.Path]::GetFullPath($ExpectedBinaryPath)

if ([IO.Path]::GetFileName($ArchivePath) -cne "viewr-windows-x64.zip") {
    throw "windows-x64 archive must be named viewr-windows-x64.zip."
}
if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
    throw "Portable archive does not exist: $ArchivePath"
}
if ((Get-Item -LiteralPath $ArchivePath).Length -eq 0) {
    throw "Portable archive is empty: $ArchivePath"
}
Assert-ZipIntegrity -Path $ArchivePath
if (-not (Test-Path -LiteralPath $ExpectedBinaryPath -PathType Leaf)) {
    throw "Expected Viewr binary does not exist: $ExpectedBinaryPath"
}
Assert-X64Pe -Path $ExpectedBinaryPath

$sourceByName =
    [Collections.Generic.Dictionary[string, string]]::new(
        [StringComparer]::Ordinal
    )
$sourceByName.Add("viewr.exe", $ExpectedBinaryPath)
$sourceByName.Add("LICENSE", (Join-Path $repositoryRoot "LICENSE"))
$sourceByName.Add(
    "THIRD-PARTY-LICENSES.txt",
    (Join-Path $repositoryRoot "packaging\THIRD-PARTY-LICENSES.txt")
)
$sourceByName.Add(
    "THIRD-PARTY-NOTICES.txt",
    (Join-Path $repositoryRoot "packaging\THIRD-PARTY-NOTICES.txt")
)
$sourceByName.Add(
    "RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html",
    (Join-Path $repositoryRoot `
        "packaging\RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html")
)
$sourceByName.Add(
    "SOURCE-BUILD.md",
    (Join-Path $repositoryRoot "packaging\SOURCE-BUILD.md")
)
$sourceByName.Add(
    "rawler-0.7.2-LICENSE",
    (Join-Path $repositoryRoot "packaging\licenses\rawler-0.7.2-LICENSE")
)

foreach ($sourcePath in $sourceByName.Values) {
    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "Required comparison file does not exist: $sourcePath"
    }
    if ((Get-Item -LiteralPath $sourcePath).Length -eq 0) {
        throw "Required comparison file is empty: $sourcePath"
    }
}

$expectedRawlerLicenseHash =
    "c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"
$actualRawlerLicenseHash = (
    Get-FileHash `
        -LiteralPath $sourceByName["rawler-0.7.2-LICENSE"] `
        -Algorithm SHA256
).Hash.ToLowerInvariant()
if ($actualRawlerLicenseHash -ne $expectedRawlerLicenseHash) {
    throw (
        "rawler 0.7.2 LICENSE hash mismatch: expected " +
        "$expectedRawlerLicenseHash, got $actualRawlerLicenseHash."
    )
}

$temporaryDirectory = Join-Path (
    [IO.Path]::GetTempPath()
) ("viewr-portable-validation-" + [Guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null

Add-Type -AssemblyName System.IO.Compression
$archive = $null
$archiveStream = $null

try {
    $archiveStream = [IO.File]::Open(
        $ArchivePath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    $archive = [IO.Compression.ZipArchive]::new(
        $archiveStream,
        [IO.Compression.ZipArchiveMode]::Read,
        $false
    )

    if ($archive.Entries.Count -ne $sourceByName.Count) {
        throw (
            "Portable archive contains $($archive.Entries.Count) entries; " +
            "expected $($sourceByName.Count)."
        )
    }

    $actualNames =
        [Collections.Generic.HashSet[string]]::new(
            [StringComparer]::Ordinal
        )
    $caseInsensitiveNames =
        [Collections.Generic.HashSet[string]]::new(
            [StringComparer]::OrdinalIgnoreCase
        )

    foreach ($entry in $archive.Entries) {
        if ([string]::IsNullOrEmpty($entry.Name) -or
            $entry.FullName -cne $entry.Name -or
            $entry.FullName.Contains("/") -or
            $entry.FullName.Contains("\")) {
            throw "Archive entry is not a flat regular file: '$($entry.FullName)'."
        }
        if (-not $actualNames.Add($entry.FullName)) {
            throw "Archive contains duplicate entry '$($entry.FullName)'."
        }
        if (-not $caseInsensitiveNames.Add($entry.FullName)) {
            throw (
                "Archive contains a case-colliding entry " +
                "'$($entry.FullName)'."
            )
        }
        if (-not $sourceByName.ContainsKey($entry.FullName)) {
            throw "Archive contains unexpected entry '$($entry.FullName)'."
        }

        $expectedLength = (
            Get-Item -LiteralPath $sourceByName[$entry.FullName]
        ).Length
        if ($entry.Length -ne $expectedLength) {
            throw (
                "Archive entry '$($entry.FullName)' has length " +
                "$($entry.Length); expected $expectedLength."
            )
        }

        $destinationPath = Join-Path $temporaryDirectory $entry.FullName
        $entryStream = $entry.Open()
        $destinationStream = $null
        try {
            $destinationStream = [IO.File]::Open(
                $destinationPath,
                [IO.FileMode]::CreateNew,
                [IO.FileAccess]::Write,
                [IO.FileShare]::None
            )
            $entryStream.CopyTo($destinationStream)
        }
        finally {
            if ($null -ne $destinationStream) {
                $destinationStream.Dispose()
            }
            $entryStream.Dispose()
        }
    }

    foreach ($expectedName in $sourceByName.Keys) {
        if (-not $actualNames.Contains($expectedName)) {
            throw "Archive is missing required entry '$expectedName'."
        }

        Assert-SameFileHash `
            -ActualPath (Join-Path $temporaryDirectory $expectedName) `
            -ExpectedPath $sourceByName[$expectedName] `
            -Description "Archive entry '$expectedName'"
    }

    Assert-X64Pe -Path (Join-Path $temporaryDirectory "viewr.exe")
}
finally {
    if ($null -ne $archive) {
        $archive.Dispose()
    }
    if ($null -ne $archiveStream) {
        $archiveStream.Dispose()
    }
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}

Write-Host "Validated $ArchivePath"
