# SPDX-FileCopyrightText: 2026 hiunivs
# SPDX-License-Identifier: AGPL-3.0-only
# Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet("x64", "arm64")][string]$Architecture,
    [Parameter(Mandatory = $true)][ValidateRange(1, 65535)][int]$BuildNumber,
    [Parameter(Mandatory = $true)][string]$Publisher,
    [Parameter(Mandatory = $true)][string]$PublisherDisplayName,
    [Parameter(Mandatory = $true)][string]$UpdateBaseUrl,
    [string]$BetaUpdateBaseUrl,
    [ValidateSet("stable", "beta")][string]$ReleaseChannel = "stable",
    [ValidatePattern('^[A-Za-z0-9.-]{3,50}$')][string]$PackageName = "com.rshell.rshell"
)

$ErrorActionPreference = "Stop"
$RootDir = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$CargoToml = Get-Content (Join-Path $RootDir "Cargo.toml") -Raw
$Match = [regex]::Match($CargoToml, '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"')
if (-not $Match.Success) { throw "Cargo package version was not found" }
$SemVer = $Match.Groups["version"].Value
$ExpectedTag = "v$SemVer"
$Parts = $SemVer.Split(".")
foreach ($Part in $Parts) {
    if ([int]$Part -gt 65535) { throw "Each Windows package version component must be <= 65535" }
}
$PackageVersion = "$($Parts[0]).$($Parts[1]).$($Parts[2]).$BuildNumber"

function Get-SecureBaseUrl([string]$Name, [string]$Value) {
    $Uri = [Uri]$Value
    if (-not $Uri.IsAbsoluteUri -or $Uri.Scheme -ne "https") {
        throw "$Name must be an absolute HTTPS URL"
    }
    if ($Uri.UserInfo -or $Uri.Fragment -or $Uri.Query) {
        throw "$Name cannot contain credentials, a query, or a fragment"
    }
    return $Value.TrimEnd("/")
}

function Assert-NativeSuccess([string]$Step) {
    if ($LASTEXITCODE -ne 0) {
        throw "$Step failed with exit code $LASTEXITCODE"
    }
}

$RequiredLegalFiles = @(
    "LICENSE",
    "NOTICE",
    "COMMERCIAL-LICENSE.md",
    "THIRD_PARTY_NOTICES.md",
    "THIRD_PARTY_LICENSES.html"
)
foreach ($LegalFile in $RequiredLegalFiles) {
    if (-not (Test-Path -PathType Leaf (Join-Path $RootDir $LegalFile))) {
        throw "Required legal file not found: $LegalFile"
    }
}

$GitStatus = (& git -C $RootDir status --porcelain --untracked-files=normal) -join ""
Assert-NativeSuccess "Checking Git worktree"
if ($GitStatus) { throw "Release builds require a clean Git worktree" }
$HeadCommit = (& git -C $RootDir rev-parse HEAD).Trim()
Assert-NativeSuccess "Reading release commit"
& git -C $RootDir show-ref --verify --quiet "refs/tags/$ExpectedTag"
if ($LASTEXITCODE -ne 0) { throw "Release tag not found: $ExpectedTag" }
$TagCommit = (& git -C $RootDir rev-parse "$ExpectedTag^{commit}").Trim()
Assert-NativeSuccess "Reading release tag"
if ($TagCommit -ne $HeadCommit) {
    throw "Release commit must be tagged exactly $ExpectedTag"
}

$StableBaseUrl = Get-SecureBaseUrl "UpdateBaseUrl" $UpdateBaseUrl
$BetaBaseUrl = $null
if ($BetaUpdateBaseUrl) {
    $BetaBaseUrl = Get-SecureBaseUrl "BetaUpdateBaseUrl" $BetaUpdateBaseUrl
}
if ($ReleaseChannel -eq "beta" -and -not $BetaBaseUrl) {
    throw "BetaUpdateBaseUrl is required when ReleaseChannel is beta"
}
$ReleaseBaseUrl = if ($ReleaseChannel -eq "beta") { $BetaBaseUrl } else { $StableBaseUrl }

$Target = if ($Architecture -eq "x64") { "x86_64-pc-windows-msvc" } else { "aarch64-pc-windows-msvc" }
if (Get-Command rustup -ErrorAction SilentlyContinue) {
    rustup target add $Target
    Assert-NativeSuccess "Installing Rust target"
}
$env:RSHELL_WINDOWS_APPINSTALLER_URL = "$StableBaseUrl/RShell.appinstaller"
if ($BetaBaseUrl) {
    $env:RSHELL_WINDOWS_BETA_APPINSTALLER_URL = "$BetaBaseUrl/RShell.appinstaller"
} else {
    Remove-Item Env:RSHELL_WINDOWS_BETA_APPINSTALLER_URL -ErrorAction SilentlyContinue
}
cargo build --locked --release --target $Target --manifest-path (Join-Path $RootDir "Cargo.toml")
Assert-NativeSuccess "Building RShell"

$DistDir = Join-Path $RootDir "dist/windows/$ReleaseChannel"
$SourceDistDir = Join-Path $RootDir "dist/source"
$StageDir = Join-Path $env:TEMP ("rshell-msix-" + [Guid]::NewGuid().ToString("N"))
$PackageDir = Join-Path $StageDir "package"
$BundleInputDir = Join-Path $StageDir "bundle"
$LicensesDir = Join-Path $PackageDir "Licenses"
New-Item -ItemType Directory -Force $PackageDir, (Join-Path $PackageDir "Assets"), $LicensesDir, $BundleInputDir, $DistDir, $SourceDistDir | Out-Null

try {
    Copy-Item (Join-Path $RootDir "target/$Target/release/rshell.exe") (Join-Path $PackageDir "rshell.exe")
    Copy-Item (Join-Path $RootDir "LICENSE") (Join-Path $LicensesDir "RShell-AGPL-3.0-only.txt")
    Copy-Item (Join-Path $RootDir "NOTICE") (Join-Path $LicensesDir "RShell-NOTICE.txt")
    Copy-Item (Join-Path $RootDir "COMMERCIAL-LICENSE.md") (Join-Path $LicensesDir "RShell-COMMERCIAL-LICENSING.md")
    Copy-Item (Join-Path $RootDir "THIRD_PARTY_NOTICES.md") (Join-Path $LicensesDir "THIRD_PARTY_NOTICES.md")
    Copy-Item (Join-Path $RootDir "THIRD_PARTY_LICENSES.html") (Join-Path $LicensesDir "THIRD_PARTY_LICENSES.html")
    Copy-Item (Join-Path $RootDir "assets/fonts/OFL.txt") (Join-Path $LicensesDir "Noto-Sans-SC-OFL-1.1.txt")
    Copy-Item (Join-Path $RootDir "assets/icons/lucide/LICENSE") (Join-Path $LicensesDir "Lucide-ISC.txt")
    $Manifest = Get-Content (Join-Path $PSScriptRoot "AppxManifest.xml") -Raw
    $Manifest = $Manifest.Replace("__PACKAGE_NAME__", [Security.SecurityElement]::Escape($PackageName))
    $Manifest = $Manifest.Replace("__PUBLISHER__", [Security.SecurityElement]::Escape($Publisher))
    $Manifest = $Manifest.Replace("__PUBLISHER_DISPLAY_NAME__", [Security.SecurityElement]::Escape($PublisherDisplayName))
    $Manifest = $Manifest.Replace("__VERSION__", $PackageVersion)
    $Manifest = $Manifest.Replace("__ARCH__", $Architecture)
    Set-Content (Join-Path $PackageDir "AppxManifest.xml") $Manifest -Encoding utf8

    Add-Type -AssemblyName System.Drawing
    $Assets = @{
        "StoreLogo.png" = 50
        "Square44x44Logo.png" = 44
        "Square150x150Logo.png" = 150
    }
    foreach ($Asset in $Assets.GetEnumerator()) {
        $Bitmap = [System.Drawing.Bitmap]::new($Asset.Value, $Asset.Value)
        $Graphics = [System.Drawing.Graphics]::FromImage($Bitmap)
        try {
            $Graphics.Clear([System.Drawing.Color]::FromArgb(42, 109, 217))
            $FontSize = [Math]::Max(16, [Math]::Floor($Asset.Value * 0.55))
            $Font = [System.Drawing.Font]::new("Segoe UI", $FontSize, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
            try {
                $Format = [System.Drawing.StringFormat]::new()
                try {
                    $Format.Alignment = [System.Drawing.StringAlignment]::Center
                    $Format.LineAlignment = [System.Drawing.StringAlignment]::Center
                    $Brush = [System.Drawing.Brushes]::White
                    $Rect = [System.Drawing.RectangleF]::new(0, 0, $Asset.Value, $Asset.Value)
                    $Graphics.DrawString("R", $Font, $Brush, $Rect, $Format)
                } finally { $Format.Dispose() }
            } finally { $Font.Dispose() }
            $Bitmap.Save((Join-Path $PackageDir "Assets/$($Asset.Key)"), [System.Drawing.Imaging.ImageFormat]::Png)
        } finally {
            $Graphics.Dispose()
            $Bitmap.Dispose()
        }
    }

    $SdkBin = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64" -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName "makeappx.exe") } |
        Sort-Object { [version]$_.Parent.Name } -Descending |
        Select-Object -First 1
    if (-not $SdkBin) { throw "Windows SDK tools were not found" }
    $MakeAppx = Join-Path $SdkBin.FullName "makeappx.exe"
    $SignTool = Join-Path $SdkBin.FullName "signtool.exe"
    if (-not (Test-Path $SignTool)) { throw "signtool.exe was not found next to makeappx.exe in $($SdkBin.FullName)" }
    $MsixName = "RShell-$SemVer-$Architecture.msix"
    $MsixPath = Join-Path $BundleInputDir $MsixName
    & $MakeAppx pack /d $PackageDir /p $MsixPath /o
    Assert-NativeSuccess "Creating MSIX"

    $BundleName = "RShell-$SemVer.msixbundle"
    $CandidateBundlePath = Join-Path $StageDir $BundleName
    & $MakeAppx bundle /d $BundleInputDir /p $CandidateBundlePath /o
    Assert-NativeSuccess "Creating MSIX bundle"

    if ($env:RSHELL_WINDOWS_CERT_SHA1) {
        & $SignTool sign /fd SHA256 /td SHA256 /tr "http://timestamp.digicert.com" `
            /sha1 $env:RSHELL_WINDOWS_CERT_SHA1 $CandidateBundlePath
        Assert-NativeSuccess "Signing MSIX bundle"
    } elseif ($env:RSHELL_WINDOWS_PFX_PATH -and $env:RSHELL_WINDOWS_PFX_PASSWORD) {
        & $SignTool sign /fd SHA256 /td SHA256 /tr "http://timestamp.digicert.com" `
            /f $env:RSHELL_WINDOWS_PFX_PATH /p $env:RSHELL_WINDOWS_PFX_PASSWORD $CandidateBundlePath
        Assert-NativeSuccess "Signing MSIX bundle"
    } else {
        throw "Set RSHELL_WINDOWS_CERT_SHA1 or the PFX path/password; unsigned update packages are never copied to dist"
    }
    # A self-signed PFX is not rooted in the machine's trust store, so a plain
    # signtool verify fails even though the signature itself is valid. Trust the
    # certificate in the current user's root store just for verification and
    # remove it immediately afterwards (CI runners are disposable).
    $ImportedRootCert = $null
    if ($env:RSHELL_WINDOWS_PFX_PATH -and $env:RSHELL_WINDOWS_PFX_PASSWORD -and -not $env:RSHELL_WINDOWS_CERT_SHA1) {
        $PfxPassword = ConvertTo-SecureString $env:RSHELL_WINDOWS_PFX_PASSWORD -AsPlainText -Force
        $ImportedRootCert = Import-PfxCertificate -FilePath $env:RSHELL_WINDOWS_PFX_PATH `
            -Password $PfxPassword -CertStoreLocation Cert:\CurrentUser\Root
    }
    try {
        & $SignTool verify /pa /v $CandidateBundlePath
        Assert-NativeSuccess "Verifying MSIX bundle signature"
    } finally {
        if ($ImportedRootCert) {
            Remove-Item -LiteralPath "Cert:\CurrentUser\Root\$($ImportedRootCert.Thumbprint)" -Force
        }
    }

    # Only a successfully signed and verified bundle may enter the publishing directory.
    $BundlePath = Join-Path $DistDir $BundleName
    Copy-Item -Force $CandidateBundlePath $BundlePath

    $BundleUrl = "$ReleaseBaseUrl/$BundleName"
    $AppInstallerUrl = "$ReleaseBaseUrl/RShell.appinstaller"
    $AppInstaller = Get-Content (Join-Path $PSScriptRoot "RShell.appinstaller") -Raw
    $AppInstaller = $AppInstaller.Replace("__PACKAGE_NAME__", [Security.SecurityElement]::Escape($PackageName))
    $AppInstaller = $AppInstaller.Replace("__PUBLISHER__", [Security.SecurityElement]::Escape($Publisher))
    $AppInstaller = $AppInstaller.Replace("__VERSION__", $PackageVersion)
    $AppInstaller = $AppInstaller.Replace("__APPINSTALLER_URL__", [Security.SecurityElement]::Escape($AppInstallerUrl))
    $AppInstaller = $AppInstaller.Replace("__BUNDLE_URL__", [Security.SecurityElement]::Escape($BundleUrl))
    Set-Content (Join-Path $DistDir "RShell.appinstaller") $AppInstaller -Encoding utf8

    # Ship the exact tagged source together with all locked Cargo dependency
    # sources needed to rebuild it.
    $SourceRoot = Join-Path $StageDir "RShell-$SemVer"
    $SourceTar = Join-Path $StageDir "rshell-source.tar"
    New-Item -ItemType Directory -Force $SourceRoot, (Join-Path $SourceRoot ".cargo") | Out-Null
    & git -C $RootDir archive --format=tar --output=$SourceTar $ExpectedTag
    Assert-NativeSuccess "Exporting tagged source"
    & tar -xf $SourceTar -C $SourceRoot
    Assert-NativeSuccess "Extracting tagged source"
    Push-Location $SourceRoot
    try {
        $VendorConfig = & cargo vendor --locked --versioned-dirs vendor
        Assert-NativeSuccess "Vendoring locked dependency sources"
        $VendorConfig | Set-Content (Join-Path $SourceRoot ".cargo/config.toml") -Encoding utf8
    } finally {
        Pop-Location
    }
    $SourceArchive = Join-Path $SourceDistDir "RShell-$SemVer-source.zip"
    & tar -a -cf $SourceArchive -C $StageDir "RShell-$SemVer"
    Assert-NativeSuccess "Creating corresponding source archive"

    Write-Host "Created $BundlePath"
    Write-Host "Created $(Join-Path $DistDir 'RShell.appinstaller')"
    Write-Host "Created $SourceArchive"
} finally {
    if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
}
