[CmdletBinding()]
param(
    [Parameter()]
    [string] $BinaryPath,

    [Parameter()]
    [string] $OutputDirectory,

    [Parameter()]
    [string] $ReleaseTag,

    [Parameter()]
    [string] $RawlerLicensePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$script:ExpectedRawlerVersion = "0.7.2"
$script:ExpectedRawlerLicenseSha256 =
    "c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"

function Invoke-NativeCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Command,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]] $Arguments
    )

    $stderrPath = [IO.Path]::GetTempFileName()
    try {
        $previousErrorActionPreference = $ErrorActionPreference
        try {
            # Windows PowerShell can promote redirected native stderr to an
            # ErrorRecord. Keep it separate from stdout so cargo metadata
            # remains valid JSON and inspect the native exit code ourselves.
            $ErrorActionPreference = "Continue"
            $output = & $Command @Arguments 2> $stderrPath
            $exitCode = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $previousErrorActionPreference
        }

        $diagnostics = [IO.File]::ReadAllText($stderrPath).TrimEnd()
        if ($exitCode -ne 0) {
            $failureOutput = @()
            $stdoutText = ($output | Out-String).TrimEnd()
            if (-not [string]::IsNullOrWhiteSpace($stdoutText)) {
                $failureOutput += $stdoutText
            }
            if (-not [string]::IsNullOrWhiteSpace($diagnostics)) {
                $failureOutput += $diagnostics
            }
            $failureDetails = $failureOutput -join [Environment]::NewLine
            throw "'$Command' exited with code $exitCode.`n$failureDetails"
        }
        if (-not [string]::IsNullOrWhiteSpace($diagnostics)) {
            Write-Host $diagnostics
        }
        return $output
    }
    finally {
        Remove-Item -LiteralPath $stderrPath -Force -ErrorAction SilentlyContinue
    }
}

function Find-WixTool {
    param(
        [Parameter(Mandatory = $true)]
        [string] $FileName
    )

    $command = Get-Command $FileName -CommandType Application -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    $programFilesX86 = ${env:ProgramFiles(x86)}
    if (-not [string]::IsNullOrWhiteSpace($programFilesX86)) {
        $candidate = Join-Path $programFilesX86 "WiX Toolset v3.14\bin\$FileName"
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return $candidate
        }
    }

    throw "WiX 3.14.1 tool '$FileName' was not found."
}

function Assert-WixVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string] $ToolPath
    )

    $versionOutput = (& $ToolPath "-?" 2>&1 | Out-String)
    if ($versionOutput -notmatch '(?i)\b3\.14\.1(?:\.\d+)?\b') {
        throw "Expected WiX 3.14.1, but '$ToolPath -?' reported:`n$versionOutput"
    }
}

function Get-ViewrPackageVersion {
    param(
        [Parameter(Mandatory = $true)]
        [object] $CargoMetadata
    )

    $packages = @($CargoMetadata.packages | Where-Object { $_.name -eq "viewr" })
    if ($packages.Count -ne 1) {
        throw "Expected exactly one Cargo package named 'viewr'; found $($packages.Count)."
    }
    return [string] $packages[0].version
}

function Assert-MsiVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Version
    )

    if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)$') {
        throw "MSI releases require a stable three-part numeric version; got '$Version'."
    }

    $major = [uint64] $Matches[1]
    $minor = [uint64] $Matches[2]
    $build = [uint64] $Matches[3]
    if ($major -gt 255 -or $minor -gt 255 -or $build -gt 65535) {
        throw (
            "MSI version '$Version' exceeds Windows Installer limits " +
            "(major/minor <= 255, build <= 65535)."
        )
    }
}

function New-DeterministicProductCode {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Version
    )

    $seed = [Text.Encoding]::UTF8.GetBytes("viewr/windows-msi/product-code/$Version")
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = $sha256.ComputeHash($seed)
    }
    finally {
        $sha256.Dispose()
    }

    $guidBytes = New-Object byte[] 16
    [Array]::Copy($digest, $guidBytes, 16)
    # MSI only requires a unique GUID. Hashing this stable, versioned seed
    # gives each release a repeatable ProductCode without persisted state.
    return ([Guid]::new($guidBytes)).ToString("B").ToUpperInvariant()
}

function Assert-X64Pe {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader $stream
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) {
            throw "'$Path' is not a PE executable (missing MZ signature)."
        }
        $stream.Position = 0x3c
        $peOffset = $reader.ReadUInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "'$Path' is not a PE executable (missing PE signature)."
        }
        $machine = $reader.ReadUInt16()
        if ($machine -ne 0x8664) {
            throw ("'$Path' is not an x64 PE executable " +
                "('8664' expected, '$('{0:X4}' -f $machine)' found).")
        }
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Assert-WindowsGuiPe {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader $stream
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) {
            throw "'$Path' is not a PE executable (missing MZ signature)."
        }
        $stream.Position = 0x3c
        $peOffset = [int64] $reader.ReadUInt32()
        if ($peOffset + 94 -gt $stream.Length) {
            throw "'$Path' has a truncated PE header."
        }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "'$Path' is not a PE executable (missing PE signature)."
        }
        $stream.Position = $peOffset + 24
        if ($reader.ReadUInt16() -ne 0x020b) {
            throw "'$Path' is not a PE32+ executable."
        }
        $stream.Position = $peOffset + 24 + 68
        $subsystem = $reader.ReadUInt16()
        if ($subsystem -ne 2) {
            throw (
                "'$Path' is not a Windows GUI executable " +
                "(subsystem 2 expected, '$subsystem' found)."
            )
        }
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Convert-PlainTextToRtf {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $Text
    )

    $normalizedText = $Text.Replace("`r`n", "`n").Replace("`r", "`n")
    $builder = [Text.StringBuilder]::new()
    [void] $builder.Append(
        "{\rtf1\ansi\deff0{\fonttbl{\f0\fswiss Segoe UI;}}`r`n"
    )
    [void] $builder.Append("\viewkind4\uc1\pard\f0\fs18`r`n\b ")
    $firstLine = $true

    foreach ($character in $normalizedText.ToCharArray()) {
        $codeUnit = [int] $character
        if ($codeUnit -eq 0x5c) {
            [void] $builder.Append("\\")
        }
        elseif ($codeUnit -eq 0x7b) {
            [void] $builder.Append("\{")
        }
        elseif ($codeUnit -eq 0x7d) {
            [void] $builder.Append("\}")
        }
        elseif ($codeUnit -eq 0x0a) {
            if ($firstLine) {
                [void] $builder.Append("\b0")
                $firstLine = $false
            }
            [void] $builder.Append("\par`r`n")
        }
        elseif ($codeUnit -eq 0x09) {
            [void] $builder.Append("\tab ")
        }
        elseif ($codeUnit -ge 0x20 -and $codeUnit -le 0x7e) {
            [void] $builder.Append($character)
        }
        else {
            $signedCodeUnit = $codeUnit
            if ($signedCodeUnit -gt 0x7fff) {
                $signedCodeUnit -= 0x10000
            }
            [void] $builder.Append("\u")
            [void] $builder.Append($signedCodeUnit)
            [void] $builder.Append("?")
        }
    }

    if ($firstLine) {
        [void] $builder.Append("\b0")
    }
    [void] $builder.Append("}`r`n")
    return $builder.ToString()
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $repositoryRoot "target\x86_64-pc-windows-msvc\release\viewr.exe"
}
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot "dist"
}

$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$wixSource = Join-Path $repositoryRoot "packaging\windows\viewr.wxs"
$launcherSource = Join-Path $repositoryRoot "packaging\windows\ViewrLauncher.rs"
$thirdPartyNoticesPath =
    Join-Path $repositoryRoot "packaging\THIRD-PARTY-NOTICES.txt"
$thirdPartyLicensesPath =
    Join-Path $repositoryRoot "packaging\THIRD-PARTY-LICENSES.txt"
$rustCopyrightPath =
    Join-Path $repositoryRoot `
        "packaging\RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
$sourceBuildPath = Join-Path $repositoryRoot "packaging\SOURCE-BUILD.md"
$viewrLicensePath = Join-Path $repositoryRoot "LICENSE"

foreach ($requiredPath in @(
        $BinaryPath,
        $wixSource,
        $launcherSource,
        $thirdPartyLicensesPath,
        $thirdPartyNoticesPath,
        $rustCopyrightPath,
        $sourceBuildPath,
        $viewrLicensePath
    )) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required file does not exist: $requiredPath"
    }
}
Assert-X64Pe -Path $BinaryPath

$cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop).Source
$manifestPath = Join-Path $repositoryRoot "Cargo.toml"
$metadataText = (
    Invoke-NativeCommand `
        $cargo `
        "metadata" `
        "--manifest-path" $manifestPath `
        "--locked" `
        "--no-deps" `
        "--format-version" "1"
) -join [Environment]::NewLine
$metadata = $metadataText | ConvertFrom-Json
$version = Get-ViewrPackageVersion -CargoMetadata $metadata
Assert-MsiVersion -Version $version

if (-not [string]::IsNullOrWhiteSpace($ReleaseTag) -and $ReleaseTag -ne "v$version") {
    throw "Release tag '$ReleaseTag' does not match Cargo package version 'v$version'."
}

if ([string]::IsNullOrWhiteSpace($RawlerLicensePath)) {
    $RawlerLicensePath =
        Join-Path $repositoryRoot "packaging\licenses\rawler-0.7.2-LICENSE"
}
$RawlerLicensePath = [IO.Path]::GetFullPath($RawlerLicensePath)
if (-not (Test-Path -LiteralPath $RawlerLicensePath -PathType Leaf)) {
    throw "rawler $($script:ExpectedRawlerVersion) license was not found: $RawlerLicensePath"
}
$rawlerLicenseHash = (
    Get-FileHash -LiteralPath $RawlerLicensePath -Algorithm SHA256
).Hash.ToLowerInvariant()
if ($rawlerLicenseHash -ne $script:ExpectedRawlerLicenseSha256) {
    throw (
        "rawler $($script:ExpectedRawlerVersion) license hash mismatch: " +
        "expected $($script:ExpectedRawlerLicenseSha256), got $rawlerLicenseHash."
    )
}

$cargoLockPath = Join-Path $repositoryRoot "Cargo.lock"
$cargoLock = Get-Content -LiteralPath $cargoLockPath -Raw
$rawlerLockPattern = (
    '(?ms)^\[\[package\]\]\r?\n' +
    'name = "rawler"\r?\n' +
    'version = "' +
    [regex]::Escape($script:ExpectedRawlerVersion) +
    '"\r?$'
)
if ($cargoLock -notmatch $rawlerLockPattern) {
    throw "Cargo.lock does not contain rawler $($script:ExpectedRawlerVersion)."
}

$candle = Find-WixTool -FileName "candle.exe"
$light = Find-WixTool -FileName "light.exe"
$rustc = (Get-Command rustc -CommandType Application -ErrorAction Stop).Source
Assert-WixVersion -ToolPath $candle
Assert-WixVersion -ToolPath $light

[IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
$temporaryDirectory =
    Join-Path ([IO.Path]::GetTempPath()) ("viewr-wix-" + [Guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null

$productCode = New-DeterministicProductCode -Version $version
$launcherBinary = Join-Path $temporaryDirectory "ViewrLauncher.exe"
$licenseRtfPath = Join-Path $temporaryDirectory "LICENSE.rtf"
$wixObject = Join-Path $temporaryDirectory "viewr.wixobj"
$msiPath = Join-Path $OutputDirectory "viewr-windows-x64.msi"
$sourceDirectory = Split-Path $BinaryPath -Parent

try {
    $licenseText = [IO.File]::ReadAllText(
        $viewrLicensePath,
        [Text.UTF8Encoding]::new($false, $true)
    )
    $licenseRtf = Convert-PlainTextToRtf -Text $licenseText
    [IO.File]::WriteAllText(
        $licenseRtfPath,
        $licenseRtf,
        [Text.Encoding]::ASCII
    )

    Invoke-NativeCommand $rustc `
        "--edition=2024" `
        "--crate-name" "viewr_launcher" `
        "--crate-type" "bin" `
        "--target" "x86_64-pc-windows-msvc" `
        "-C" "opt-level=3" `
        "-C" "panic=abort" `
        "-C" "target-feature=+crt-static" `
        "-C" "strip=symbols" `
        "-o" $launcherBinary `
        $launcherSource | Out-Host
    Assert-X64Pe -Path $launcherBinary
    Assert-WindowsGuiPe -Path $launcherBinary

    Invoke-NativeCommand $candle `
        "-nologo" `
        "-wx" `
        "-arch" "x64" `
        "-dProductCode=$productCode" `
        "-dProductVersion=$version" `
        "-dSourceDir=$sourceDirectory" `
        "-dLauncherPath=$launcherBinary" `
        "-dLicenseRtfPath=$licenseRtfPath" `
        "-dViewrLicensePath=$viewrLicensePath" `
        "-dThirdPartyLicensesPath=$thirdPartyLicensesPath" `
        "-dThirdPartyNoticesPath=$thirdPartyNoticesPath" `
        "-dRustCopyrightPath=$rustCopyrightPath" `
        "-dRawlerLicensePath=$RawlerLicensePath" `
        "-dSourceBuildPath=$sourceBuildPath" `
        "-out" $wixObject `
        $wixSource | Out-Host

    Invoke-NativeCommand $light `
        "-nologo" `
        "-wx" `
        "-spdb" `
        "-ext" "WixUIExtension" `
        "-cultures:en-us" `
        "-out" $msiPath `
        $wixObject | Out-Host
}
finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}

if (-not (Test-Path -LiteralPath $msiPath -PathType Leaf)) {
    throw "WiX completed without creating the expected MSI: $msiPath"
}

$msiHash = (Get-FileHash -LiteralPath $msiPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Built $msiPath"
Write-Host "Version: $version"
Write-Host "ProductCode: $productCode"
Write-Host "SHA256: $msiHash"
