[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("windows-x64")]
    [string] $Platform,

    [Parameter(Mandatory = $true)]
    [string] $BinaryPath,

    [Parameter(Mandatory = $true)]
    [string] $OutputPath
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

if ($Platform -ne "windows-x64") {
    throw "Unsupported platform '$Platform'."
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
$OutputPath = [IO.Path]::GetFullPath($OutputPath)

if ([IO.Path]::GetFileName($OutputPath) -cne "viewr-windows-x64.zip") {
    throw "windows-x64 output must be named viewr-windows-x64.zip."
}
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Viewr binary does not exist: $BinaryPath"
}
Assert-X64Pe -Path $BinaryPath

$sourceByName =
    [Collections.Generic.Dictionary[string, string]]::new(
        [StringComparer]::Ordinal
    )
$sourceByName.Add("viewr.exe", $BinaryPath)
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
        throw "Required source file does not exist: $sourcePath"
    }
    if ((Get-Item -LiteralPath $sourcePath).Length -eq 0) {
        throw "Required source file is empty: $sourcePath"
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

$outputDirectory = [IO.Path]::GetDirectoryName($OutputPath)
[IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
$temporaryDirectory = Join-Path (
    [IO.Path]::GetTempPath()
) ("viewr-portable-" + [Guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$temporaryArchive = Join-Path $temporaryDirectory "viewr-windows-x64.zip"

Add-Type -AssemblyName System.IO.Compression

$archive = $null
$archiveStream = $null
try {
    $archiveStream = [IO.File]::Open(
        $temporaryArchive,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    $archive = [IO.Compression.ZipArchive]::new(
        $archiveStream,
        [IO.Compression.ZipArchiveMode]::Create,
        $false
    )
    try {
        $memberNames = [string[]] @(
            "LICENSE"
            "RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
            "SOURCE-BUILD.md"
            "THIRD-PARTY-LICENSES.txt"
            "THIRD-PARTY-NOTICES.txt"
            "rawler-0.7.2-LICENSE"
            "viewr.exe"
        )
        $fixedTimestamp = [DateTimeOffset]::new(
            1980,
            1,
            1,
            0,
            0,
            0,
            [TimeSpan]::Zero
        )

        foreach ($memberName in $memberNames) {
            $entry = $archive.CreateEntry(
                $memberName,
                [IO.Compression.CompressionLevel]::Optimal
            )
            $entry.LastWriteTime = $fixedTimestamp
            $entryStream = $entry.Open()
            $sourceStream = $null
            try {
                $sourceStream = [IO.File]::Open(
                    $sourceByName[$memberName],
                    [IO.FileMode]::Open,
                    [IO.FileAccess]::Read,
                    [IO.FileShare]::Read
                )
                $sourceStream.CopyTo($entryStream)
            }
            finally {
                if ($null -ne $sourceStream) {
                    $sourceStream.Dispose()
                }
                $entryStream.Dispose()
            }
        }
    }
    finally {
        if ($null -ne $archive) {
            $archive.Dispose()
            $archive = $null
        }
        if ($null -ne $archiveStream) {
            $archiveStream.Dispose()
            $archiveStream = $null
        }
    }

    if (Test-Path -LiteralPath $OutputPath) {
        Remove-Item -LiteralPath $OutputPath -Force
    }
    Move-Item -LiteralPath $temporaryArchive -Destination $OutputPath
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

if (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
    throw "Portable archive was not created: $OutputPath"
}
if ((Get-Item -LiteralPath $OutputPath).Length -eq 0) {
    throw "Portable archive is empty: $OutputPath"
}

Write-Host "Created $OutputPath"
