[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $MsiPath,

    [Parameter()]
    [string] $ExpectedBinaryPath,

    [Parameter()]
    [switch] $AllowMachineChanges
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not $AllowMachineChanges.IsPresent) {
    throw (
        "This test installs an MSI and changes HKLM. Run it only on a disposable " +
        "Windows test machine, and pass -AllowMachineChanges to continue."
    )
}

$script:Hklm = $null
$script:InstalledProductCode = $null
$script:InstallAttempted = $false
$script:TestCompleted = $false
$script:ExpectedRawlerLicenseSha256 =
    "c1228ae47a5ada0464e9cc2f1c253e2437432866570b9ac6244bceb4d75c0f10"

function Assert-True {
    param(
        [Parameter(Mandatory = $true)]
        [bool] $Condition,

        [Parameter(Mandatory = $true)]
        [string] $Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Assert-Equal {
    param(
        [AllowNull()]
        [object] $Actual,

        [AllowNull()]
        [object] $Expected,

        [Parameter(Mandatory = $true)]
        [string] $Message
    )

    if ($Actual -ne $Expected) {
        throw "$Message Expected '$Expected', got '$Actual'."
    }
}

function Assert-PathEqual {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Actual,

        [Parameter(Mandatory = $true)]
        [string] $Expected,

        [Parameter(Mandatory = $true)]
        [string] $Message
    )

    $actualFull = [IO.Path]::GetFullPath($Actual).TrimEnd('\')
    $expectedFull = [IO.Path]::GetFullPath($Expected).TrimEnd('\')
    if (-not $actualFull.Equals($expectedFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Message Expected '$expectedFull', got '$actualFull'."
    }
}

function Assert-X64Pe {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object System.IO.BinaryReader $stream
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
            throw (
                "'$Path' is not an x64 PE executable " +
                "('8664' expected, '$('{0:X4}' -f $machine)' found)."
            )
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
    $reader = New-Object System.IO.BinaryReader $stream
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

function Find-Dumpbin {
    $command = Get-Command `
        dumpbin.exe `
        -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }

    $programFilesX86 = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
        throw "dumpbin.exe is required, but ProgramFiles(x86) is not set."
    }
    $vswhere = Join-Path `
        $programFilesX86 `
        "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "dumpbin.exe is required, and vswhere.exe was not found."
    }

    $candidates = @(
        & $vswhere `
            -latest `
            -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -find "VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe"
    )
    if ($LASTEXITCODE -ne 0) {
        throw "vswhere.exe exited with code $LASTEXITCODE while locating dumpbin.exe."
    }
    foreach ($candidate in $candidates) {
        if (
            -not [string]::IsNullOrWhiteSpace($candidate) -and
            (Test-Path -LiteralPath $candidate -PathType Leaf)
        ) {
            return [IO.Path]::GetFullPath($candidate)
        }
    }
    throw "dumpbin.exe is required, but Visual Studio C++ tools were not found."
}

function Assert-NoDynamicCrtImports {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [string] $DumpbinPath
    )

    $dumpbinOutput = (& $DumpbinPath /NOLOGO /DEPENDENTS $Path 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin /DEPENDENTS failed for '$Path' with code $LASTEXITCODE."
    }
    $dependencies = @(
        $dumpbinOutput -split '\r?\n' |
            ForEach-Object { $_.Trim() } |
            Where-Object { $_ -match '(?i)\.dll$' }
    )
    if ($dependencies.Count -eq 0) {
        throw "dumpbin did not report any DLL dependencies for '$Path'."
    }
    $dynamicCrtImports = @(
        $dependencies |
            Where-Object {
                $_ -match (
                    '(?i)^(?:VCRUNTIME[^\\/\s]*\.dll|' +
                    'MSVCP[^\\/\s]*\.dll|UCRTBASE\.dll|' +
                    'api-ms-win-crt-[^\\/\s]*\.dll)$'
                )
            } |
            Sort-Object -Unique
    )
    if ($dynamicCrtImports.Count -ne 0) {
        throw (
            "'$Path' imports a dynamic MSVC runtime: " +
            ($dynamicCrtImports -join ", ")
        )
    }
}

function Assert-ViewrUsage {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Path
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Could not start installed Viewr executable: $Path"
        }
        if (-not $process.WaitForExit(30000)) {
            $process.Kill()
            throw "Installed Viewr did not print usage and exit within 30 seconds."
        }
        $standardOutput = $process.StandardOutput.ReadToEnd()
        $standardError = $process.StandardError.ReadToEnd()
        Assert-Equal $process.ExitCode 0 `
            "Installed Viewr returned a nonzero exit code with no arguments."
        $combinedOutput = $standardOutput + [Environment]::NewLine + $standardError
        Assert-True `
            ($combinedOutput.Contains("usage: viewr <folder|file.arw>")) `
            "Installed Viewr did not print the expected usage text."
    }
    finally {
        $process.Dispose()
    }
}

function Assert-MsiActionSucceeded {
    param(
        [Parameter(Mandatory = $true)]
        [string] $LogPath,

        [Parameter(Mandatory = $true)]
        [string] $Action
    )

    if (-not (Test-Path -LiteralPath $LogPath -PathType Leaf)) {
        throw "MSI log does not exist: $LogPath"
    }
    $log = Get-Content -LiteralPath $LogPath -Raw
    $escapedAction = [regex]::Escape($Action)
    Assert-True `
        ($log -match "(?m)^Action start .*: $escapedAction\.\r?$") `
        "MSI log does not show custom action '$Action' starting."
    Assert-True `
        ($log -match (
            "(?m)^Action ended .*: $escapedAction\. Return value 1\.\r?$"
        )) `
        "MSI log does not show custom action '$Action' succeeding."
}

function Get-RegistryValueState {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $Name
    )

    $key = $script:Hklm.OpenSubKey($Path, $false)
    if ($null -eq $key) {
        return [pscustomobject] @{
            KeyExisted = $false
            ValueExisted = $false
            Value = $null
            Kind = $null
        }
    }

    try {
        $valueExisted = @($key.GetValueNames()) -contains $Name
        if (-not $valueExisted) {
            return [pscustomobject] @{
                KeyExisted = $true
                ValueExisted = $false
                Value = $null
                Kind = $null
            }
        }
        return [pscustomobject] @{
            KeyExisted = $true
            ValueExisted = $true
            Value = $key.GetValue(
                $Name,
                $null,
                [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
            )
            Kind = $key.GetValueKind($Name)
        }
    }
    finally {
        $key.Dispose()
    }
}

function Get-RegistryValue {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $Name
    )

    $state = Get-RegistryValueState -Path $Path -Name $Name
    if (-not $state.ValueExisted) {
        throw "Registry value is missing: HKLM:\$Path [$Name]"
    }
    return $state.Value
}

function Set-RegistryValue {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $Name,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [object] $Value,

        [Parameter(Mandatory = $true)]
        [Microsoft.Win32.RegistryValueKind] $Kind
    )

    $key = $script:Hklm.CreateSubKey($Path, $true)
    try {
        $key.SetValue($Name, $Value, $Kind)
    }
    finally {
        $key.Dispose()
    }
}

function Restore-RegistryValue {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string] $Name,

        [Parameter(Mandatory = $true)]
        [object] $State
    )

    if ($State.ValueExisted) {
        Set-RegistryValue -Path $Path -Name $Name -Value $State.Value -Kind $State.Kind
        return
    }

    $key = $script:Hklm.OpenSubKey($Path, $true)
    if ($null -ne $key) {
        try {
            $key.DeleteValue($Name, $false)
        }
        finally {
            $key.Dispose()
        }
    }
}

function Remove-RegistryKeyIfEmpty {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $key = $script:Hklm.OpenSubKey($Path, $true)
    if ($null -eq $key) {
        return
    }
    try {
        if ($key.ValueCount -ne 0 -or $key.SubKeyCount -ne 0) {
            return
        }
    }
    finally {
        $key.Dispose()
    }

    $separator = $Path.LastIndexOf('\')
    if ($separator -le 0) {
        return
    }
    $parentPath = $Path.Substring(0, $separator)
    $leafName = $Path.Substring($separator + 1)
    $parent = $script:Hklm.OpenSubKey($parentPath, $true)
    if ($null -ne $parent) {
        try {
            $parent.DeleteSubKey($leafName, $false)
        }
        finally {
            $parent.Dispose()
        }
    }
}

function Test-RegistryKey {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $key = $script:Hklm.OpenSubKey($Path, $false)
    if ($null -eq $key) {
        return $false
    }
    $key.Dispose()
    return $true
}

function Assert-RegistryKeyContainsOnlySubKey {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path,

        [Parameter(Mandatory = $true)]
        [string] $SubKeyName
    )

    $key = $script:Hklm.OpenSubKey($Path, $false)
    if ($null -eq $key) {
        throw "Registry key is missing: HKLM:\$Path"
    }
    try {
        $valueNames = @($key.GetValueNames())
        $subKeyNames = @($key.GetSubKeyNames())
        Assert-Equal $valueNames.Count 0 `
            "HKLM:\$Path contains an unexpected value after uninstall."
        Assert-Equal $subKeyNames.Count 1 `
            "HKLM:\$Path contains an unexpected subkey after uninstall."
        Assert-Equal $subKeyNames[0] $SubKeyName `
            "HKLM:\$Path did not preserve only the unrelated sentinel."
    }
    finally {
        $key.Dispose()
    }
}

function Get-ViewrArpEntries {
    $uninstallPath = "Software\Microsoft\Windows\CurrentVersion\Uninstall"
    $uninstall = $script:Hklm.OpenSubKey($uninstallPath, $false)
    if ($null -eq $uninstall) {
        return @()
    }

    $entries = @()
    try {
        foreach ($subKeyName in $uninstall.GetSubKeyNames()) {
            $entry = $uninstall.OpenSubKey($subKeyName, $false)
            if ($null -eq $entry) {
                continue
            }
            try {
                if ([string] $entry.GetValue("DisplayName") -eq "Viewr") {
                    $entries += [pscustomobject] @{
                        ProductCode = $subKeyName
                        DisplayVersion = [string] $entry.GetValue("DisplayVersion")
                        Publisher = [string] $entry.GetValue("Publisher")
                        WindowsInstaller = $entry.GetValue("WindowsInstaller")
                        NoModify = $entry.GetValue("NoModify")
                    }
                }
            }
            finally {
                $entry.Dispose()
            }
        }
    }
    finally {
        $uninstall.Dispose()
    }
    return @($entries)
}

function Invoke-MsiExec {
    param(
        [Parameter(Mandatory = $true)]
        [ValidateSet("install", "uninstall")]
        [string] $Operation,

        [Parameter(Mandatory = $true)]
        [string] $Target,

        [Parameter(Mandatory = $true)]
        [string] $LogPath
    )

    if ($Target.Contains('"') -or $LogPath.Contains('"')) {
        throw "MSI target and log paths must not contain quotation marks."
    }
    $switch = if ($Operation -eq "install") { "/i" } else { "/x" }
    $arguments = @(
        $switch,
        ('"{0}"' -f $Target),
        "/qn",
        "/norestart",
        "/L*v",
        ('"{0}"' -f $LogPath)
    )
    $process = Start-Process `
        -FilePath (Join-Path $env:SystemRoot "System32\msiexec.exe") `
        -ArgumentList $arguments `
        -Wait `
        -PassThru
    if ($process.ExitCode -notin @(0, 3010)) {
        throw (
            "msiexec $Operation failed with exit code $($process.ExitCode). " +
            "See $LogPath."
        )
    }
}

if (-not [Environment]::Is64BitOperatingSystem -or -not [Environment]::Is64BitProcess) {
    throw "The x64 MSI test must run in a 64-bit PowerShell process on 64-bit Windows."
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal $identity
$isAdministrator = $principal.IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)
if (-not $isAdministrator) {
    throw "The MSI integration test requires an elevated PowerShell process."
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$MsiPath = [IO.Path]::GetFullPath($MsiPath)
if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf)) {
    throw "MSI does not exist: $MsiPath"
}
if ([string]::IsNullOrWhiteSpace($ExpectedBinaryPath)) {
    $ExpectedBinaryPath = Join-Path `
        $repositoryRoot `
        "target\x86_64-pc-windows-msvc\release\viewr.exe"
}
$ExpectedBinaryPath = [IO.Path]::GetFullPath($ExpectedBinaryPath)
if (-not (Test-Path -LiteralPath $ExpectedBinaryPath -PathType Leaf)) {
    throw "Expected Viewr executable does not exist: $ExpectedBinaryPath"
}
Assert-X64Pe -Path $ExpectedBinaryPath
$dumpbin = Find-Dumpbin
Assert-NoDynamicCrtImports `
    -Path $ExpectedBinaryPath `
    -DumpbinPath $dumpbin
$expectedBinarySha256 = (
    Get-FileHash -LiteralPath $ExpectedBinaryPath -Algorithm SHA256
).Hash

$cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop).Source
$manifestPath = Join-Path $repositoryRoot "Cargo.toml"
$cargoMetadataText = (
    & $cargo `
        metadata `
        --manifest-path $manifestPath `
        --locked `
        --no-deps `
        --format-version 1
) -join [Environment]::NewLine
if ($LASTEXITCODE -ne 0) {
    throw "cargo metadata exited with code $LASTEXITCODE."
}
$cargoMetadata = $cargoMetadataText | ConvertFrom-Json
$viewrPackages = @($cargoMetadata.packages | Where-Object { $_.name -eq "viewr" })
if ($viewrPackages.Count -ne 1) {
    throw "Expected exactly one Cargo package named 'viewr'."
}
$expectedVersion = [string] $viewrPackages[0].version

$expectedExe = Join-Path $env:ProgramFiles "Viewr\viewr.exe"
$expectedLauncher = Join-Path $env:ProgramFiles "Viewr\ViewrLauncher.exe"
$expectedInstallDirectory = Split-Path $expectedExe -Parent
$expectedCommand = '"' + $expectedLauncher + '" "%1"'
$expectedIcon = '"' + $expectedExe + '",0'
$shortcutPath = Join-Path $env:ProgramData `
    "Microsoft\Windows\Start Menu\Programs\Viewr\Viewr.lnk"
$licensesDirectory = Join-Path $expectedInstallDirectory "licenses"
$viewrLicense = Join-Path $licensesDirectory "Viewr-LICENSE.txt"
$thirdPartyLicenses = Join-Path $licensesDirectory "THIRD-PARTY-LICENSES.txt"
$thirdPartyNotices = Join-Path $licensesDirectory "THIRD-PARTY-NOTICES.txt"
$rustCopyright = Join-Path `
    $licensesDirectory `
    "RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
$rawlerLicense = Join-Path $licensesDirectory "rawler-0.7.2-LICENSE.txt"
$sourceBuildInstructions = Join-Path $licensesDirectory "SOURCE-BUILD.md"

$arwPath = "Software\Classes\.arw"
$openWithPath = "$arwPath\OpenWithProgids"
$sentinelProgId = "ViewrInstallerTest.Unrelated"
$sentinelProgIdPath = "Software\Classes\$sentinelProgId"
$viewrRootPath = "Software\Viewr"
$viewrRootSentinelName = "ViewrInstallerTest.Unrelated"
$viewrRootSentinelPath = "$viewrRootPath\$viewrRootSentinelName"
$viewrRootSentinelToken = [Guid]::NewGuid().ToString("N")
$defaultArwState = $null
$openWithState = $null
$arwKeyExisted = $false
$openWithKeyExisted = $false
$sentinelCreated = $false
$viewrRootSentinelCreated = $false
$installLog = Join-Path ([IO.Path]::GetTempPath()) `
    ("viewr-msi-install-" + [Guid]::NewGuid().ToString("N") + ".log")
$uninstallLog = Join-Path ([IO.Path]::GetTempPath()) `
    ("viewr-msi-uninstall-" + [Guid]::NewGuid().ToString("N") + ".log")

$script:Hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
    [Microsoft.Win32.RegistryHive]::LocalMachine,
    [Microsoft.Win32.RegistryView]::Registry64
)
$cleanupFailures = @()

try {
    $existingArpEntries = @(Get-ViewrArpEntries)
    if ($existingArpEntries.Count -ne 0) {
        throw "Refusing to test over an existing Viewr installation."
    }
    if (Test-RegistryKey -Path $sentinelProgIdPath) {
        throw "The test sentinel ProgID already exists: $sentinelProgIdPath"
    }
    if (Test-RegistryKey -Path $viewrRootSentinelPath) {
        throw "The test Viewr-root sentinel already exists: $viewrRootSentinelPath"
    }
    $registeredApplicationBefore = Get-RegistryValueState `
        -Path "Software\RegisteredApplications" `
        -Name "Viewr"
    $viewrOpenWithBefore = Get-RegistryValueState `
        -Path $openWithPath `
        -Name "Viewr.ARW"
    if (
        (Test-Path -LiteralPath $expectedInstallDirectory) -or
        (Test-Path -LiteralPath $shortcutPath) -or
        (Test-RegistryKey -Path "Software\Classes\Viewr.ARW") -or
        (Test-RegistryKey `
            -Path "Software\Classes\Applications\ViewrLauncher.exe") -or
        (Test-RegistryKey -Path $viewrRootPath) -or
        (Test-RegistryKey `
            -Path "Software\Microsoft\Windows\CurrentVersion\App Paths\viewr.exe") -or
        $registeredApplicationBefore.ValueExisted -or
        $viewrOpenWithBefore.ValueExisted
    ) {
        throw "Refusing to test over existing Viewr files or registration."
    }

    $defaultArwState = Get-RegistryValueState -Path $arwPath -Name ""
    $openWithState =
        Get-RegistryValueState -Path $openWithPath -Name $sentinelProgId
    $arwKeyExisted = $defaultArwState.KeyExisted
    $openWithKeyExisted = $openWithState.KeyExisted

    $viewrRootSentinelCreated = $true
    Set-RegistryValue `
        -Path $viewrRootSentinelPath `
        -Name "Token" `
        -Value $viewrRootSentinelToken `
        -Kind String
    Set-RegistryValue `
        -Path $arwPath `
        -Name "" `
        -Value $sentinelProgId `
        -Kind String
    Set-RegistryValue `
        -Path $openWithPath `
        -Name $sentinelProgId `
        -Value "" `
        -Kind String
    $sentinelCreated = $true
    Set-RegistryValue `
        -Path $sentinelProgIdPath `
        -Name "" `
        -Value "Unrelated ARW test handler" `
        -Kind String
    Set-RegistryValue `
        -Path "$sentinelProgIdPath\shell\open\command" `
        -Name "" `
        -Value '"%SystemRoot%\System32\notepad.exe" "%1"' `
        -Kind ExpandString

    $script:InstallAttempted = $true
    Invoke-MsiExec `
        -Operation install `
        -Target $MsiPath `
        -LogPath $installLog
    Assert-MsiActionSucceeded `
        -LogPath $installLog `
        -Action "NotifyAssociationsAfterInstall"
    Assert-Equal `
        (Get-RegistryValue -Path $viewrRootSentinelPath -Name "Token") `
        $viewrRootSentinelToken `
        "Installer changed the unrelated HKLM Software\Viewr sentinel."

    $arpEntries = @(Get-ViewrArpEntries)
    Assert-Equal $arpEntries.Count 1 "Unexpected Viewr ARP entry count."
    $script:InstalledProductCode = $arpEntries[0].ProductCode
    Assert-Equal $arpEntries[0].DisplayVersion $expectedVersion `
        "Wrong ARP display version."
    Assert-Equal $arpEntries[0].Publisher "Hunter Chen" "Wrong ARP publisher."
    Assert-Equal $arpEntries[0].WindowsInstaller 1 "ARP entry is not an MSI product."
    Assert-Equal $arpEntries[0].NoModify 1 `
        "ARP entry must disable unsupported modify operations."

    Assert-True (Test-Path -LiteralPath $expectedExe -PathType Leaf) `
        "Installed executable is missing."
    Assert-True (Test-Path -LiteralPath $expectedLauncher -PathType Leaf) `
        "Installed GUI launcher is missing."
    Assert-X64Pe -Path $expectedExe
    Assert-X64Pe -Path $expectedLauncher
    Assert-WindowsGuiPe -Path $expectedLauncher
    Assert-NoDynamicCrtImports `
        -Path $expectedExe `
        -DumpbinPath $dumpbin
    Assert-NoDynamicCrtImports `
        -Path $expectedLauncher `
        -DumpbinPath $dumpbin
    $installedBinarySha256 = (
        Get-FileHash -LiteralPath $expectedExe -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedBinarySha256 `
        $expectedBinarySha256 `
        "Installed executable differs from the packaged executable."
    Assert-ViewrUsage -Path $expectedExe
    Assert-True (Test-Path -LiteralPath $viewrLicense -PathType Leaf) `
        "Installed Viewr license is missing."
    Assert-True (Test-Path -LiteralPath $thirdPartyLicenses -PathType Leaf) `
        "Installed third-party license inventory is missing."
    Assert-True (Test-Path -LiteralPath $thirdPartyNotices -PathType Leaf) `
        "Installed third-party notice is missing."
    Assert-True (Test-Path -LiteralPath $rustCopyright -PathType Leaf) `
        "Installed Rust standard-library copyright inventory is missing."
    Assert-True (Test-Path -LiteralPath $rawlerLicense -PathType Leaf) `
        "Installed rawler license is missing."
    Assert-True (Test-Path -LiteralPath $sourceBuildInstructions -PathType Leaf) `
        "Installed source-build instructions are missing."

    $expectedViewrLicenseHash = (
        Get-FileHash -LiteralPath (Join-Path $repositoryRoot "LICENSE") -Algorithm SHA256
    ).Hash
    $installedViewrLicenseHash = (
        Get-FileHash -LiteralPath $viewrLicense -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedViewrLicenseHash `
        $expectedViewrLicenseHash `
        "Installed Viewr license differs from the repository license."
    $expectedNoticeHash = (
        Get-FileHash `
            -LiteralPath (
                Join-Path $repositoryRoot "packaging\THIRD-PARTY-NOTICES.txt"
            ) `
            -Algorithm SHA256
    ).Hash
    $installedNoticeHash = (
        Get-FileHash -LiteralPath $thirdPartyNotices -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedNoticeHash `
        $expectedNoticeHash `
        "Installed third-party notice differs from the packaging source."
    $expectedLicensesHash = (
        Get-FileHash `
            -LiteralPath (
                Join-Path $repositoryRoot "packaging\THIRD-PARTY-LICENSES.txt"
            ) `
            -Algorithm SHA256
    ).Hash
    $installedLicensesHash = (
        Get-FileHash -LiteralPath $thirdPartyLicenses -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedLicensesHash `
        $expectedLicensesHash `
        "Installed third-party licenses differ from the generated inventory."
    $expectedRustCopyrightHash = (
        Get-FileHash `
            -LiteralPath (
                Join-Path `
                    $repositoryRoot `
                    "packaging\RUST-1.96-STANDARD-LIBRARY-COPYRIGHT.html"
            ) `
            -Algorithm SHA256
    ).Hash
    $installedRustCopyrightHash = (
        Get-FileHash -LiteralPath $rustCopyright -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedRustCopyrightHash `
        $expectedRustCopyrightHash `
        "Installed Rust notices differ from the pinned toolchain copy."
    $installedRawlerLicenseHash = (
        Get-FileHash -LiteralPath $rawlerLicense -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    Assert-Equal `
        $installedRawlerLicenseHash `
        $script:ExpectedRawlerLicenseSha256 `
        "Installed rawler license is not the exact rawler 0.7.2 license."
    $expectedSourceBuildHash = (
        Get-FileHash `
            -LiteralPath (Join-Path $repositoryRoot "packaging\SOURCE-BUILD.md") `
            -Algorithm SHA256
    ).Hash
    $installedSourceBuildHash = (
        Get-FileHash -LiteralPath $sourceBuildInstructions -Algorithm SHA256
    ).Hash
    Assert-Equal `
        $installedSourceBuildHash `
        $expectedSourceBuildHash `
        "Installed source-build instructions differ from the packaging source."

    Assert-Equal `
        (Get-RegistryValue -Path $arwPath -Name "") `
        $sentinelProgId `
        "Installer changed the existing .arw default."
    Assert-True `
        (Get-RegistryValueState -Path $openWithPath -Name $sentinelProgId).ValueExisted `
        "Installer removed the unrelated OpenWithProgids entry."
    Assert-True `
        (Get-RegistryValueState -Path $openWithPath -Name "Viewr.ARW").ValueExisted `
        "Viewr OpenWithProgids entry is missing."

    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Viewr.ARW" `
            -Name "") `
        "Sony ARW raw image" `
        "Viewr ProgID description is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Viewr.ARW" `
            -Name "FriendlyTypeName") `
        "Sony ARW raw image" `
        "Viewr ProgID friendly type name is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Viewr.ARW\shell\open\command" `
            -Name "") `
        $expectedCommand `
        "Viewr ProgID open command is not safely quoted."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Viewr.ARW\DefaultIcon" `
            -Name "") `
        $expectedIcon `
        "Viewr ProgID icon command is not safely quoted."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Applications\ViewrLauncher.exe\shell\open\command" `
            -Name "") `
        $expectedCommand `
        "Applications open command is not safely quoted."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Applications\ViewrLauncher.exe" `
            -Name "FriendlyAppName") `
        "Viewr" `
        "Applications friendly name is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Classes\Applications\ViewrLauncher.exe\SupportedTypes" `
            -Name ".arw") `
        "" `
        "Applications SupportedTypes is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Viewr\Capabilities" `
            -Name "ApplicationName") `
        "Viewr" `
        "Capabilities application name is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Viewr\Capabilities" `
            -Name "ApplicationDescription") `
        "View and rate Sony ARW raw images" `
        "Capabilities application description is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\Viewr\Capabilities\FileAssociations" `
            -Name ".arw") `
        "Viewr.ARW" `
        "Capabilities file association is incorrect."
    Assert-Equal `
        (Get-RegistryValue `
            -Path "Software\RegisteredApplications" `
            -Name "Viewr") `
        "Software\Viewr\Capabilities" `
        "RegisteredApplications entry is incorrect."
    Assert-PathEqual `
        (Get-RegistryValue `
            -Path "Software\Microsoft\Windows\CurrentVersion\App Paths\viewr.exe" `
            -Name "") `
        $expectedExe `
        "App Paths executable is incorrect."
    Assert-PathEqual `
        (Get-RegistryValue `
            -Path "Software\Microsoft\Windows\CurrentVersion\App Paths\viewr.exe" `
            -Name "Path") `
        $expectedInstallDirectory `
        "App Paths search directory is incorrect."

    Assert-True (Test-Path -LiteralPath $shortcutPath -PathType Leaf) `
        "Start Menu shortcut is missing."
    $shell = New-Object -ComObject WScript.Shell
    try {
        $shortcut = $shell.CreateShortcut($shortcutPath)
        Assert-PathEqual $shortcut.TargetPath $expectedLauncher `
            "Start Menu shortcut target is incorrect."
        Assert-Equal $shortcut.Arguments "--pick-folder" `
            "Start Menu shortcut arguments are incorrect."
        Assert-PathEqual $shortcut.WorkingDirectory $expectedInstallDirectory `
            "Start Menu shortcut working directory is incorrect."
    }
    finally {
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null
    }

    Invoke-MsiExec `
        -Operation uninstall `
        -Target $script:InstalledProductCode `
        -LogPath $uninstallLog
    $script:InstalledProductCode = $null
    Assert-MsiActionSucceeded `
        -LogPath $uninstallLog `
        -Action "NotifyAssociationsAfterRemove"

    Assert-True (-not (Test-Path -LiteralPath $expectedExe)) `
        "Uninstall left viewr.exe behind."
    Assert-True (-not (Test-Path -LiteralPath $expectedLauncher)) `
        "Uninstall left ViewrLauncher.exe behind."
    Assert-True (-not (Test-Path -LiteralPath $shortcutPath)) `
        "Uninstall left the Start Menu shortcut behind."
    Assert-True (-not (Test-Path -LiteralPath $expectedInstallDirectory)) `
        "Uninstall left the Viewr installation directory behind."
    $remainingArpEntries = @(Get-ViewrArpEntries)
    Assert-Equal $remainingArpEntries.Count 0 `
        "Uninstall left a Viewr ARP entry behind."
    Assert-True (-not (Test-RegistryKey -Path "Software\Classes\Viewr.ARW")) `
        "Uninstall left the Viewr ProgID behind."
    Assert-True `
        (-not (Test-RegistryKey `
            -Path "Software\Classes\Applications\ViewrLauncher.exe")) `
        "Uninstall left the Applications registration behind."
    Assert-True `
        (-not (Test-RegistryKey `
            -Path "Software\Microsoft\Windows\CurrentVersion\App Paths\viewr.exe")) `
        "Uninstall left the App Paths registration behind."
    Assert-True `
        (-not (Test-RegistryKey -Path "Software\Viewr\Capabilities")) `
        "Uninstall left the Viewr capabilities registration behind."
    $startMenuShortcutAfter = Get-RegistryValueState `
        -Path $viewrRootPath `
        -Name "StartMenuShortcut"
    Assert-True `
        (-not $startMenuShortcutAfter.ValueExisted) `
        "Uninstall left the MSI-owned StartMenuShortcut value behind."
    Assert-Equal `
        (Get-RegistryValue -Path $viewrRootSentinelPath -Name "Token") `
        $viewrRootSentinelToken `
        "Uninstall removed the unrelated HKLM Software\Viewr sentinel."
    Assert-RegistryKeyContainsOnlySubKey `
        -Path $viewrRootPath `
        -SubKeyName $viewrRootSentinelName
    $registeredApplicationAfter = Get-RegistryValueState `
        -Path "Software\RegisteredApplications" `
        -Name "Viewr"
    Assert-True `
        (-not $registeredApplicationAfter.ValueExisted) `
        "Uninstall left the RegisteredApplications value behind."
    $viewrOpenWithAfter = Get-RegistryValueState `
        -Path $openWithPath `
        -Name "Viewr.ARW"
    Assert-True `
        (-not $viewrOpenWithAfter.ValueExisted) `
        "Uninstall left the Viewr OpenWithProgids value behind."
    Assert-Equal `
        (Get-RegistryValue -Path $arwPath -Name "") `
        $sentinelProgId `
        "Uninstall changed the unrelated .arw default."
    Assert-True `
        (Get-RegistryValueState -Path $openWithPath -Name $sentinelProgId).ValueExisted `
        "Uninstall removed the unrelated OpenWithProgids entry."

    $script:TestCompleted = $true
}
finally {
    $cleanupEntries = @()
    if ($null -ne $script:InstalledProductCode) {
        try {
            Invoke-MsiExec `
                -Operation uninstall `
                -Target $script:InstalledProductCode `
                -LogPath $uninstallLog
        }
        catch {
            $message = "Cleanup uninstall failed: $_"
            Write-Warning $message
            $cleanupFailures += $message
        }
    }
    else {
        if ($script:InstallAttempted) {
            try {
                $cleanupEntries = @(Get-ViewrArpEntries)
            }
            catch {
                $message = "Could not inspect installed products for cleanup: $_"
                Write-Warning $message
                $cleanupFailures += $message
            }
        }
    }
    if ($null -eq $script:InstalledProductCode -and $cleanupEntries.Count -ne 0) {
        try {
            $cleanupEntry = $cleanupEntries[0]
            Invoke-MsiExec `
                -Operation uninstall `
                -Target $cleanupEntry.ProductCode `
                -LogPath $uninstallLog
        }
        catch {
            $message = "Fallback cleanup uninstall failed: $_"
            Write-Warning $message
            $cleanupFailures += $message
        }
    }

    if ($null -ne $script:Hklm) {
        try {
            if ($null -ne $openWithState) {
                try {
                    Restore-RegistryValue `
                        -Path $openWithPath `
                        -Name $sentinelProgId `
                        -State $openWithState
                }
                catch {
                    $message = "Could not restore the OpenWith sentinel: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }
            if ($null -ne $defaultArwState) {
                try {
                    Restore-RegistryValue `
                        -Path $arwPath `
                        -Name "" `
                        -State $defaultArwState
                }
                catch {
                    $message = "Could not restore the .arw default: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }

            if ($sentinelCreated) {
                try {
                    if (Test-RegistryKey -Path $sentinelProgIdPath) {
                        $script:Hklm.DeleteSubKeyTree($sentinelProgIdPath, $false)
                    }
                }
                catch {
                    $message = "Could not remove test sentinel ProgID: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }
            if ($viewrRootSentinelCreated) {
                try {
                    if (Test-RegistryKey -Path $viewrRootSentinelPath) {
                        $script:Hklm.DeleteSubKeyTree($viewrRootSentinelPath, $false)
                    }
                    Remove-RegistryKeyIfEmpty -Path $viewrRootPath
                }
                catch {
                    $message = "Could not remove the Viewr-root test sentinel: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }
            if ($null -ne $openWithState -and -not $openWithKeyExisted) {
                try {
                    Remove-RegistryKeyIfEmpty -Path $openWithPath
                }
                catch {
                    $message = "Could not remove the test OpenWith key: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }
            if ($null -ne $defaultArwState -and -not $arwKeyExisted) {
                try {
                    Remove-RegistryKeyIfEmpty -Path $arwPath
                }
                catch {
                    $message = "Could not remove the test .arw key: $_"
                    Write-Warning $message
                    $cleanupFailures += $message
                }
            }
        }
        finally {
            try {
                $script:Hklm.Dispose()
            }
            catch {
                $message = "Could not close the HKLM registry handle: $_"
                Write-Warning $message
                $cleanupFailures += $message
            }
            finally {
                $script:Hklm = $null
            }
        }
    }
    if ($script:TestCompleted -and $cleanupFailures.Count -ne 0) {
        throw (
            "The MSI test passed, but cleanup was incomplete: " +
            ($cleanupFailures -join " | ")
        )
    }
}

Write-Host "Windows MSI install/uninstall integration test passed."
Write-Host "Install log: $installLog"
Write-Host "Uninstall log: $uninstallLog"
