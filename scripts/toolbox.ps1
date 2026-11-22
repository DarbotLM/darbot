#requires -Version 5.1

<#
.SYNOPSIS
Build, install or launch the Windows x64 Darbot desktop release. No action by default.
.DESCRIPTION
This is the desktop release toolbox, not the repository inventory/SMB toolbox.
Help and Doctor do not install anything. Doctor reports local presence, not sign-in,
provider compatibility or live connectivity. Use the provider's official consent flow.
Prerequisites defaults to Runtime (WebView2). Build tools are an explicit opt-in.
Prerequisites may download packages, accept their installer agreements and require UAC.
Build and Package run the checkout's code: use only a trusted, exclusively controlled checkout and
toolchain. It uses a new target on every run, never cargo clean or recursive deletion.
Keep the printed installer and remove old toolbox build directories yourself as needed.
Build/Package require desktop\dist to be absent; move prior frontend output aside first.
They use the normal relative ../dist and beforeBuildCommand without Tauri overrides.
BuiltExeSHA256 describes the raw build output, NOT the installed NSIS payload: Tauri
may restore its UNK bundle marker after packaging an NSS-marked executable.
For Install, obtain ExpectedExeSha256 independently from the release coordinator's verified payload.
-Offline restricts Cargo dependency resolution; Bun and Tauri's pinned bundler tools
can still need network access. No publishing, provider setup or live tests are requested.
Package produces the same optimized installer as Build, without running regression suites.
It does not mark a release as qualified or publish it.
Build/Package -WithMsix also package the unchanged raw built executable as a real MSIX.
Msix packages an existing trusted ApplicationPath after ExpectedVersion/ExpectedExeSha256
verification. In Msix mode that digest identifies the supplied executable, not an NSIS payload.
MSIX requires an already installed WebView2 Runtime; it does not provision that runtime.
Specify -AllowUnsignedMsix for an explicitly unsigned artifact (NOT generally install-ready),
or -MsixCertificateThumbprint for an existing CurrentUser\My code-signing certificate whose
subject matches -MsixPublisher. No certificates are created or trusted by this script.
An optional -MsixTimestampUrl must be an explicitly trusted HTTPS RFC3161 service.
MsixOutputDirectory is an optional local output parent; each run creates a unique directory.
Install needs no checkout or build tools. Supply the ORIGINAL darbot_X.Y.Z_x64-setup.exe
and both installer and installed-executable SHA256 values from a trusted release channel,
not hashes calculated from the download or the existing installation.
Close Darbot (including the tray) and keep it closed until installation finishes.
Install and Launch support the normal, non-elevated, default per-user installation only.
After verification, Install stores the version and installer/installed-exe digests under
HKCU\Software\darbotlm\Toolbox. -ReplaceSameVersion explicitly permits replacing
an existing version without a matching receipt. A verified identical pair is skipped.
Exit 0 = success/no action needed, nonzero = invalid input, failed or refused action.
.EXAMPLE
.\toolbox.ps1 Help
.EXAMPLE
.\toolbox.ps1 Doctor
.EXAMPLE
.\toolbox.ps1 Prerequisites -For Runtime
.EXAMPLE
.\scripts\toolbox.ps1 Prerequisites -For Build
.EXAMPLE
.\scripts\toolbox.ps1 Build -Offline
.EXAMPLE
.\scripts\toolbox.ps1 Package -Offline
.EXAMPLE
.\scripts\toolbox.ps1 Package -Offline -WithMsix -AllowUnsignedMsix
.EXAMPLE
.\scripts\toolbox.ps1 Msix -ApplicationPath .\darbot-desktop.exe -ExpectedVersion 0.0.27 -ExpectedExeSha256 $trustedBuildHash -AllowUnsignedMsix
.EXAMPLE
.\toolbox.ps1 Install -InstallerPath .\darbot_0.0.27_x64-setup.exe -ExpectedVersion 0.0.27 -ExpectedSha256 $trustedReleaseHash -ExpectedExeSha256 $trustedExeHash
.EXAMPLE
.\toolbox.ps1 Launch
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Position = 0)]
    [ValidateSet('Help','Doctor','Prerequisites','Build','Package','Msix','Install','Launch')]
    [string]$Mode = 'Help',
    [ValidateSet('Runtime','Build')] [string]$For = 'Runtime',
    [switch]$Offline,
    [string]$InstallerPath,
    [string]$ExpectedVersion,
    [string]$ExpectedSha256,
    [string]$ExpectedExeSha256,
    [switch]$ReplaceSameVersion,
    [switch]$WithMsix,
    [string]$ApplicationPath,
    [string]$MsixOutputDirectory,
    [string]$MsixPublisher = 'CN=Darbot',
    [switch]$AllowUnsignedMsix,
    [string]$MsixCertificateThumbprint,
    [string]$MsixTimestampUrl
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$triple = 'x86_64-pc-windows-msvc'
$stableVersion = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z'
$arpKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\darbot'
$receipt = 'HKEY_CURRENT_USER\Software\darbotlm\Toolbox'

function Find-Tool([string[]]$Names) {
    foreach ($name in $Names) {
        $cmd = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($cmd) { return $cmd.Source }
    }
}

function Invoke-Checked([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    Assert-ExitCode "$([IO.Path]::GetFileName($Program)) $($Arguments[0])" $LASTEXITCODE
}

function Assert-ExitCode([string]$Action, [int]$Code) {
    if ($Code -eq 0) { return }
    $exception = [InvalidOperationException]::new("$Action failed (exit $Code).")
    $exception.Data['ExitCode'] = $Code
    throw $exception
}

function Update-ProcessPath {
    $paths = @($env:Path, [Environment]::GetEnvironmentVariable('Path','Machine'),
        [Environment]::GetEnvironmentVariable('Path','User'),
        (Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo\bin'),
        (Join-Path ([Environment]::GetFolderPath('UserProfile')) '.bun\bin'))
    $env:Path = (($paths -split ';' | Where-Object { $_ } | Select-Object -Unique) -join ';')
}

function Assert-LocalPath([string]$Path) {
    if ($Path -notmatch '^[a-z]:\\' -or $Path.Substring(2) -match '[:*?"<>|]' -or
        [IO.DriveInfo]::new($Path.Substring(0,3)).DriveType -notin 'Fixed','Removable') {
        throw 'A normal local drive path is required (no network, device or alternate-stream paths).'
    }
    $p = [IO.Path]::GetPathRoot($Path)
    foreach ($part in $Path.Substring($p.Length).Split('\', [StringSplitOptions]::RemoveEmptyEntries)) {
        if ($part -match '[. ]$|^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\.|$)') {
            throw 'Ambiguous Windows path component.'
        }
        $p = [IO.Path]::Combine($p, $part)
        try { $attr = [IO.File]::GetAttributes($p) }
        catch [IO.FileNotFoundException] { break }
        catch [IO.DirectoryNotFoundException] { break }
        if ($attr -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a reparse point in a product path.' }
    }
}

# Inspect each directory entry before descending; never follow links in a tree
# which Vite may empty or NSIS may overwrite.
function Assert-PlainTree([IO.DirectoryInfo]$Directory) {
    foreach ($entry in $Directory.EnumerateFileSystemInfos()) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a linked output tree.' }
        if ($entry -is [IO.DirectoryInfo]) { Assert-PlainTree $entry }
    }
}

function Test-WebView {
    $subkey = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    foreach ($root in 'HKEY_CURRENT_USER\Software','HKEY_LOCAL_MACHINE\Software\WOW6432Node','HKEY_LOCAL_MACHINE\Software') {
        $v = [version]'0.0'
        if ([version]::TryParse([string][Microsoft.Win32.Registry]::GetValue("$root\$subkey",'pv',''),[ref]$v) -and
            $v -gt [version]'0.0.0.0') { return $true }
    }
    return $false
}

function Get-VSPath([switch]$RequireCompiler) {
    $where = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $where -PathType Leaf) {
        $components = @()
        if ($RequireCompiler) { $components = @('-requires','Microsoft.VisualStudio.Component.VC.Tools.x86.x64') }
        $path = & $where -latest -products '*' -version '[17.0,)' @components -property installationPath
        Assert-ExitCode 'Visual Studio inventory' $LASTEXITCODE
        if ($path) { return $path }
    }
}

function Test-Sdk {
    $kits = [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows Kits\Installed Roots','KitsRoot10',$null)
    if ($kits -and (Test-Path -LiteralPath "$kits\Lib" -PathType Container)) {
        foreach ($v in Get-ChildItem -LiteralPath "$kits\Lib" -Directory -Filter '10.*') {
            if ((Test-Path -LiteralPath "$($v.FullName)\um\x64\kernel32.lib") -and
                (Test-Path -LiteralPath "$($v.FullName)\ucrt\x64\ucrt.lib") -and
                (Test-Path -LiteralPath "$kits\bin\$($v.Name)\x64\rc.exe")) { return $true }
        }
    }
    return $false
}

function Show-Doctor {
    'Local Windows x64 inventory (no provider, account or network probes):'
    foreach ($name in 'winget','bun','rustup','rustc','cargo','gh','copilot','claude','codex','grok','pac','az') {
        $found = [bool](Find-Tool @("$name.exe","$name.cmd","$name.bat"))
        "  ${name}: $(if ($found) {'on PATH'} else {'not found on PATH'})"
    }
    "  Visual C++ x64 tools (VS 2022+): $([bool](Get-VSPath -RequireCompiler))"
    "  Windows SDK (x64 libs/resource compiler): $(Test-Sdk)"
    "  WebView2 Runtime registered: $(Test-WebView)"
    'Optional CLI presence does not establish Darbot integration, protocol compatibility, permission or sign-in.'
}

function Install-PackageId([string]$Id, [string[]]$Extra = @()) {
    $winget = Find-Tool 'winget.exe'
    if (!$winget) { throw 'Microsoft App Installer (winget) is needed to install a missing prerequisite.' }
    Write-Host "Installing missing prerequisite: $Id (winget source)."
    Invoke-Checked $winget (@('install','--id',$Id,'--exact','--source','winget','--disable-interactivity',
        '--accept-source-agreements','--accept-package-agreements') + $Extra)
    Update-ProcessPath
}

function Test-Rust {
    $rustc = Find-Tool 'rustc.exe'
    $cargo = Find-Tool 'cargo.exe'
    if (!$rustc -or !$cargo) { return $false }
    # A rustup proxy may exist without a selected compiler; never trigger an
    # implicit toolchain download just to check it.
    $ErrorActionPreference = 'Continue'
    $info = & $rustc -vV 2>$null
    if ($LASTEXITCODE -ne 0 -or $info -notcontains "host: $triple" -or
        ($info -join "`n") -notmatch '(?m)^release: (\d+\.\d+\.\d+)' -or [version]$Matches[1] -lt [version]'1.94.0') {
        return $false
    }
    $null = & $cargo --version 2>$null
    return ($LASTEXITCODE -eq 0)
}

function Assert-BuildTools([switch]$ProvisionRust) {
    $bun = Find-Tool @('bun.exe','bun.cmd')
    if (!$bun -or !(Get-VSPath -RequireCompiler) -or !(Test-Sdk)) {
        throw 'Missing build tools. Run Prerequisites -For Build and open a fresh x64 shell.'
    }
    $bv = & $bun --version
    if ($LASTEXITCODE -ne 0 -or "$bv" -notmatch '^(\d+\.\d+\.\d+)' -or [version]$Matches[1] -lt [version]'1.3.0') {
        throw 'Use Bun 1.3 or newer (desktop CI uses 1.3.14).'
    }
    $env:RUSTUP_AUTO_INSTALL = '0'
    if (Test-Rust) { return }
    $rustup = Find-Tool 'rustup.exe'
    if (!$rustup -and $ProvisionRust -and !(Find-Tool 'rustc.exe')) {
        Install-PackageId 'Rustlang.Rustup'
        $rustup = Find-Tool 'rustup.exe'
        if (Test-Rust) { return }
    }
    if ($rustup -and !$env:RUSTUP_TOOLCHAIN) {
        # Reuse an installed compatible MSVC toolchain without changing defaults.
        $local = & $rustup toolchain list
        if ($LASTEXITCODE -ne 0) { throw 'Unable to inspect installed Rust toolchains.' }
        foreach ($line in $local) {
            if ($line -match "^([a-zA-Z0-9_.-]+-$triple)(\s|$)") {
                $env:RUSTUP_TOOLCHAIN = $Matches[1]
                if (Test-Rust) { return }
            }
        }
        if ($ProvisionRust) {
            $env:RUSTUP_TOOLCHAIN = "1.94.0-$triple"
            Invoke-Checked $rustup @('toolchain','install',$env:RUSTUP_TOOLCHAIN,'--profile','minimal','--no-self-update')
            if (Test-Rust) { return }
        }
    }
    throw "A working Rust 1.94+ $triple toolchain is required. Check PATH/toolchain selection or run Prerequisites -For Build."
}

function Install-Prerequisites {
    Update-ProcessPath
    if (!(Test-WebView)) { Install-PackageId 'Microsoft.EdgeWebView2Runtime' }
    if (!(Test-WebView)) { throw 'WebView2 installation not confirmed; finish setup before retrying.' }
    if ($For -eq 'Build') {
        $components = @()
        if (!(Get-VSPath -RequireCompiler)) { $components += '--add Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
        if (!(Test-Sdk)) { $components += '--add Microsoft.VisualStudio.Component.Windows11SDK.26100' }
        if ($components.Count) {
            $vs = Get-VSPath
            if ($vs) {
                Write-Host 'Adding missing C++/SDK components to existing Visual Studio.'
                $setup = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Microsoft Visual Studio\Installer\setup.exe'
                $proc = Start-Process -FilePath $setup -ArgumentList "modify --installPath `"$vs`" $components --passive --norestart" `
                    -WorkingDirectory ([Environment]::SystemDirectory) -Wait -PassThru
                try { Assert-ExitCode 'VS Installer (finish setup/reboot if required)' $proc.ExitCode }
                finally { $proc.Dispose() }
            } else {
                Install-PackageId 'Microsoft.VisualStudio.2022.BuildTools' @('--override',"--quiet --wait --norestart $components")
            }
        }
        if (!(Find-Tool @('bun.exe','bun.cmd'))) { Install-PackageId 'Oven-sh.Bun' @('--version','1.3.14') }
        Update-ProcessPath
        Assert-BuildTools -ProvisionRust
    }
    Write-Host "$For prerequisites present. No provider setup performed."
}

function Get-ProductVersion([string]$Path) {
    $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
    $v = "$($info.ProductMajorPart).$($info.ProductMinorPart).$($info.ProductBuildPart)"
    if ($info.ProductName -ine 'darbot' -or $info.FileDescription -ine 'darbot' -or
        $info.ProductPrivatePart -ne 0 -or $info.ProductVersion -notin $v,"$v.0" -or
        $info.FileVersion -notin $v,"$v.0") { throw 'Unexpected or missing Darbot PE product/version resources.' }
    return $v
}

function Assert-X64Application([string]$Path) {
    $r = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($r.ReadUInt16() -ne 0x5A4D) { throw 'Missing application DOS signature.' }
        $r.BaseStream.Position = 60
        $pe = $r.ReadInt32()
        if ($pe -lt 64 -or $pe -gt $r.BaseStream.Length - 26) { throw 'Invalid application PE offset.' }
        $r.BaseStream.Position = $pe
        if ($r.ReadUInt32() -ne 0x4550 -or $r.ReadUInt16() -ne 0x8664) {
            throw 'Expected a Windows x64 application PE.'
        }
        $r.BaseStream.Position = $pe + 22
        $characteristics = $r.ReadUInt16()
        if (!($characteristics -band 2) -or ($characteristics -band 0x2000)) {
            throw 'Expected an executable application, not a DLL.'
        }
        if ($r.ReadUInt16() -ne 0x20B) { throw 'Expected a PE32+ application.' }
    } finally { $r.Dispose() }
}

# Darbot's app and setup have the same VERSIONINFO. Distinguish the actual
# Tauri/NSIS self-extractor by its archive header just after the PE sections.
# This is a format check, not a substitute for an independently trusted hash.
function Assert-NsisBundle([string]$Path) {
    $r = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        $s = $r.BaseStream
        if ($r.ReadUInt16() -ne 0x5A4D) { throw 'Missing setup DOS signature.' }
        $s.Position = 60
        $pe = $r.ReadInt32()
        if ($pe -lt 64 -or $pe -gt $s.Length - 24) { throw 'Invalid setup PE offset.' }
        $s.Position = $pe
        if ($r.ReadUInt32() -ne 0x4550) { throw 'Missing PE signature.' }
        $s.Position = $pe + 6
        $count = $r.ReadUInt16()
        $s.Position = $pe + 20
        $sections = $pe + 24 + $r.ReadUInt16()
        if (!$count -or $sections + 40L * $count -gt $s.Length) { throw 'Invalid setup PE sections.' }
        $end = 0L
        for ($i = 0; $i -lt $count; $i++) {
            $s.Position = $sections + 40 * $i + 16
            $length = $r.ReadUInt32()
            $end = [Math]::Max($end, ([long]$r.ReadUInt32() + $length))
        }
        if ($end -lt $sections + 40L * $count -or $end -gt $s.Length - 28) {
            throw 'Invalid NSIS archive offset.'
        }
        $s.Position = $end + 4
        if ($r.ReadUInt32() -ne 0xDEADBEEFL -or [Text.Encoding]::ASCII.GetString($r.ReadBytes(12)) -cne 'NullsoftInst') {
            throw 'Expected a Darbot NSIS setup, not an application executable.'
        }
    } finally { $r.Dispose() }
}

function Get-MsixTools {
    if (!$MsixCertificateThumbprint -and !$AllowUnsignedMsix) {
        throw 'MSIX requires -MsixCertificateThumbprint or explicit -AllowUnsignedMsix (not install-ready).'
    }
    if ($MsixCertificateThumbprint -and $AllowUnsignedMsix) { throw 'Choose signed or unsigned MSIX, not both.' }
    if ([string]::IsNullOrWhiteSpace($MsixPublisher)) { throw 'MSIX publisher is required.' }
    $null = [Security.Cryptography.X509Certificates.X500DistinguishedName]::new($MsixPublisher)
    if ($MsixTimestampUrl) {
        $uri = $null
        if (!$MsixCertificateThumbprint -or ![Uri]::TryCreate($MsixTimestampUrl,[UriKind]::Absolute,[ref]$uri) -or
            $uri.Scheme -ne 'https' -or $uri.UserInfo -or $uri.Fragment) {
            throw 'Timestamping requires signing and an explicitly trusted HTTPS RFC3161 URL without credentials.'
        }
    }
    if ($MsixCertificateThumbprint) {
        if ($MsixCertificateThumbprint -notmatch '\A[0-9a-fA-F]{40}\z') { throw 'Invalid signing certificate thumbprint.' }
        $cert = Get-Item -LiteralPath "Cert:\CurrentUser\My\$MsixCertificateThumbprint"
        if (!$cert.HasPrivateKey -or $cert.Subject -cne $MsixPublisher -or
            $cert.NotBefore -gt (Get-Date) -or $cert.NotAfter -le (Get-Date) -or
            '1.3.6.1.5.5.7.3.3' -notin @($cert.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.Value })) {
            throw 'Signing certificate must be current, have a private key and Code Signing EKU, and exactly match MsixPublisher.'
        }
    }
    $kits = [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows Kits\Installed Roots','KitsRoot10',$null)
    if (!$kits) { $kits = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Windows Kits\10' }
    $bin = Join-Path $kits 'bin'
    Assert-LocalPath $bin
    foreach ($sdk in Get-ChildItem -LiteralPath $bin -Directory -Filter '10.*' | Sort-Object { [version]$_.Name } -Descending) {
        $make = Join-Path $sdk.FullName 'x64\makeappx.exe'
        $sign = Join-Path $sdk.FullName 'x64\signtool.exe'
        if ((Test-Path -LiteralPath $make -PathType Leaf) -and
            (!$MsixCertificateThumbprint -or (Test-Path -LiteralPath $sign -PathType Leaf))) {
            Assert-LocalPath $make
            Assert-LocalPath $sign
            return @{ MakeAppx = $make; SignTool = $sign }
        }
    }
    throw 'Install the Windows SDK x64 MakeAppx/SignTool tools before packaging MSIX.'
}

function New-DarbotMsix([string]$Source, [string]$Version, [string]$SourceHash) {
    $tools = Get-MsixTools
    if (!$Source -or $Version -notmatch $stableVersion -or $SourceHash -notmatch '\A[0-9a-fA-F]{64}\z') {
        throw 'Msix requires ApplicationPath, stable ExpectedVersion and an independently trusted ExpectedExeSha256.'
    }
    foreach ($part in $Version.Split('.')) {
        if ([decimal]$part -gt 65535) { throw 'MSIX version components must be at most 65535.' }
    }
    $sourcePath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Source)
    Assert-LocalPath $sourcePath
    if ([IO.Path]::GetFileName($sourcePath) -ine 'darbot-desktop.exe') { throw 'Expected darbot-desktop.exe, not an installer.' }
    $root = [IO.Path]::GetDirectoryName($PSScriptRoot)
    $assets = Join-Path $root 'desktop\src-tauri\icons'
    $output = $MsixOutputDirectory
    if (!$output) { $output = Join-Path $root 'desktop\src-tauri\target' }
    $output = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($output)
    Assert-LocalPath $output
    $run = Join-Path $output ('msix-' + [guid]::NewGuid().ToString('N'))
    Assert-LocalPath $run
    if (Test-Path -LiteralPath $run) { throw 'MSIX output already exists; refusing reuse.' }
    # Hold the source read-only through packing; copy only the embedded app and explicit logos.
    # Never sweep build directories, user profiles or session data into the package.
    $stream = [IO.File]::Open($sourcePath,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
    try {
        Assert-X64Application $sourcePath
        if ((Get-ProductVersion $sourcePath) -ne $Version -or
            (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ine $SourceHash) {
            throw 'MSIX source version or SHA256 differs from the trusted expected executable.'
        }
        $stage = Join-Path $run 'staging'
        $null = New-Item -Path "$stage\Assets" -ItemType Directory
        Assert-LocalPath $stage
        $payload = Join-Path $stage 'darbot-desktop.exe'
        [IO.File]::Copy($sourcePath,$payload,$false)
        if ((Get-FileHash -LiteralPath $payload -Algorithm SHA256).Hash -ine $SourceHash) { throw 'MSIX payload copy hash mismatch.' }
        Add-Type -AssemblyName System.Drawing
        foreach ($logo in @(@('Square44x44Logo.png',44),@('Square150x150Logo.png',150),@('StoreLogo.png',50))) {
            $path = Join-Path $assets $logo[0]
            Assert-LocalPath $path
            [IO.File]::Copy($path,(Join-Path "$stage\Assets" $logo[0]),$false)
            $image = [Drawing.Image]::FromFile((Join-Path "$stage\Assets" $logo[0]))
            try {
                if ($image.Width -ne $logo[1] -or $image.Height -ne $logo[1]) { throw "Unexpected dimensions for $($logo[0])." }
            } finally { $image.Dispose() }
        }
        $publisher = [Security.SecurityElement]::Escape($MsixPublisher)
        $manifest = @"
<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
 xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
 xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
 IgnorableNamespaces="uap rescap">
 <Identity Name="Darbot.Desktop" Publisher="$publisher" Version="$Version.0" ProcessorArchitecture="x64" />
 <Properties><DisplayName>Darbot</DisplayName><PublisherDisplayName>Darbot</PublisherDisplayName><Logo>Assets\StoreLogo.png</Logo></Properties>
 <Dependencies><TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.17763.0" MaxVersionTested="10.0.26100.0" /></Dependencies>
 <Resources><Resource Language="en-us" /></Resources>
 <Applications><Application Id="Darbot" Executable="darbot-desktop.exe" EntryPoint="Windows.FullTrustApplication">
  <uap:VisualElements DisplayName="Darbot" Description="Darbot desktop" BackgroundColor="transparent"
   Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" />
 </Application></Applications>
 <Capabilities><rescap:Capability Name="runFullTrust" /></Capabilities>
</Package>
"@
        [IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'),$manifest,[Text.UTF8Encoding]::new($false))
        $label = if ($MsixCertificateThumbprint) { 'signed' } else { 'unsigned' }
        $package = Join-Path $run "darbot_${Version}_x64_${label}.msix"
        Assert-PlainTree ([IO.DirectoryInfo]$stage)
        Invoke-Checked $tools.MakeAppx @('pack','/d',$stage,'/p',$package,'/o','/h','SHA256')
        if ($MsixCertificateThumbprint) {
            $args = @('sign','/sha1',$MsixCertificateThumbprint,'/s','My','/fd','SHA256')
            if ($MsixTimestampUrl) { $args += @('/tr',$MsixTimestampUrl,'/td','SHA256') }
            Invoke-Checked $tools.SignTool ($args + @($package))
            Invoke-Checked $tools.SignTool @('verify','/pa','/v',$package)
        }
        Assert-LocalPath $package
        "MsixPath: $package"
        "MsixVersion: $Version.0"
        "MsixBytes: $((Get-Item -LiteralPath $package).Length)"
        "MsixSHA256: $((Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash)"
        "MsixPayloadSHA256: $SourceHash"
        "MsixSigningStatus: $(if ($MsixCertificateThumbprint) {'Signed; local Authenticode trust verified; target must trust publisher.'} else {'UNSIGNED - NOT generally install-ready.'})"
        'MSIX requires an existing WebView2 Runtime. No installation, certificate creation or trust-store changes were performed.'
    } finally { $stream.Dispose() }
}

function Invoke-Build([switch]$PackageOnly) {
    if ($WithMsix) { $null = Get-MsixTools }
    # Never interpret an inherited target as a directory to clean. Reject
    # ambiguous configurations before installing dependencies or creating output.
    foreach ($key in 'CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR','CARGO_BUILD_DIR','CARGO_BUILD_BUILD_DIR',
        'CARGO_BUILD_TARGET','TAURI_CONFIG','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_BUILD_RUSTFLAGS',
        'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS','RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER',
        'CARGO_BUILD_RUSTC_WRAPPER','CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER') {
        if ([Environment]::GetEnvironmentVariable($key)) { throw "Unset $key in the build shell; no output was cleaned." }
    }
    if (Test-Path Env:\DARBOT_LIVE_*) { throw 'Unset Darbot live-test opt-ins before running release regressions.' }
    $root = [IO.Path]::GetDirectoryName($PSScriptRoot)
    if ([IO.Path]::GetFileName($PSScriptRoot) -ine 'scripts' -or $root -eq [IO.Path]::GetPathRoot($root)) {
        throw 'Build requires scripts\toolbox.ps1 in a trusted Darbot checkout.'
    }
    $desktop = Join-Path $root 'desktop'
    $native = Join-Path $desktop 'src-tauri'
    foreach ($f in 'package.json','bun.lock','src-tauri\tauri.conf.json','src-tauri\Cargo.toml','src-tauri\Cargo.lock') {
        $p = Join-Path $desktop $f
        Assert-LocalPath $p
        if (!(Test-Path -LiteralPath $p -PathType Leaf)) { throw "Missing desktop\$f." }
    }
    $pkg = Get-Content -Raw -LiteralPath "$desktop\package.json" | ConvertFrom-Json
    $conf = Get-Content -Raw -LiteralPath "$native\tauri.conf.json" | ConvertFrom-Json
    $v = $conf.version
    $cargoPackage = [regex]::Match([IO.File]::ReadAllText("$native\Cargo.toml"),'(?ms)^\[package\]\s*(.*?)(?=^\[|\z)').Groups[1].Value
    if ($pkg.name -ne 'darbot-desktop' -or $conf.productName -ne 'darbot' -or $conf.identifier -ne 'ai.darbotlm.darbot.desktop' -or
        $v -notmatch $stableVersion -or $pkg.version -ne $v -or
        $cargoPackage -notmatch ('(?m)^version\s*=\s*"' + [regex]::Escape($v) + '"\s*$')) {
        throw 'Darbot identity or desktop package/Tauri/Cargo release versions disagree; reconcile the manifests first.'
    }
    if ($conf.build.frontendDist -cne '../dist' -or $conf.build.beforeBuildCommand -cne 'bun run build') {
        throw 'Build requires the normal relative ../dist and bun run build Tauri configuration.'
    }
    $dist = Join-Path $desktop 'dist'
    Assert-LocalPath $dist
    if (Test-Path -LiteralPath $dist) { throw 'Move existing desktop\dist aside before building; refusing to overwrite prior frontend output.' }
    Assert-LocalPath "$desktop\node_modules"
    $target = Join-Path $native ('target\tb-' + [guid]::NewGuid().ToString('N'))
    Assert-LocalPath $target
    if (Test-Path -LiteralPath $target) { throw 'Build target already exists; refusing reuse.' }
    if ($target -match '[%!\^&|<>]') { throw 'Build checkout path contains command-shell metacharacters.' }
    Update-ProcessPath
    Assert-BuildTools
    $bun = Find-Tool @('bun.exe','bun.cmd')
    $cargo = Find-Tool 'cargo.exe'
    $null = New-Item -Path $target -ItemType Directory -ErrorAction Stop
    Assert-LocalPath $target
    # This target is new even if an earlier build of this exact version failed.
    $env:CARGO_TARGET_DIR = $target
    $env:CARGO_BUILD_BUILD_DIR = $target
    $cargoConfig = Join-Path $target 'toolbox.cargo.toml'
    $targetValue = ConvertTo-Json -InputObject $target -Compress
    $cargoSettings = @('[build]', "target-dir = $targetValue", "build-dir = $targetValue",
        'rustc-wrapper = ""', 'rustc-workspace-wrapper = ""', 'incremental = false') -join "`n"
    [IO.File]::WriteAllText($cargoConfig, $cargoSettings, [Text.UTF8Encoding]::new($false))
    $cargoArgs = @('--locked','--config',$cargoConfig)
    if ($Offline) { $cargoArgs += '--offline'; $env:CARGO_NET_OFFLINE = 'true' }
    $env:COPILOT_SKIP_CLI_DOWNLOAD = '1'
    $installer = Join-Path $target "$triple\release\bundle\nsis\darbot_${v}_x64-setup.exe"
    $app = Join-Path $target "$triple\release\darbot-desktop.exe"
    Push-Location -LiteralPath $desktop
    try {
        Invoke-Checked $bun @('install','--frozen-lockfile')
        foreach ($bin in 'tsc','tauri','vite') {
            if (!(Test-Path -LiteralPath "node_modules\.bin\$bin.exe") -and
                !(Test-Path -LiteralPath "node_modules\.bin\$bin.cmd") -and
                !(Test-Path -LiteralPath "node_modules\.bin\$bin")) { throw "Missing locked desktop binary: $bin." }
        }
        $tauri = Join-Path $desktop 'node_modules\.bin\tauri.exe'
        if (!(Test-Path -LiteralPath $tauri -PathType Leaf)) {
            $tauri = Join-Path $desktop 'node_modules\.bin\tauri.cmd'
        }
        Assert-LocalPath $tauri
        if (!(Test-Path -LiteralPath $tauri -PathType Leaf)) { throw 'Missing installed local Windows Tauri CLI launcher.' }
        Invoke-Checked $bun @('run','--bun','--no-install','typecheck')
        if (!$PackageOnly) { Invoke-Checked $bun @('test','./src') }
        Assert-LocalPath $dist
        if (Test-Path -LiteralPath $dist) { throw 'desktop\dist appeared before the build; refusing to overwrite it.' }
        Invoke-Checked $tauri (@('build','--target',$triple,'--bundles','nsis','--ci','--') + $cargoArgs)
        Assert-LocalPath $dist
        if (!(Test-Path -LiteralPath "$dist\index.html" -PathType Leaf) -or
            !(Test-Path -LiteralPath "$dist\assets" -PathType Container)) {
            throw 'Normal frontend build did not produce dist\index.html and dist\assets.'
        }
        Assert-PlainTree ([IO.DirectoryInfo]$dist)
        Set-Location -LiteralPath $native
        # Candidate build.rs embeds the Common Controls manifest for MSVC.
        # Do not inject test-only linker flags into the release environment.
        if (!$PackageOnly) { Invoke-Checked $cargo (@('test','--lib','--bins','--target',$triple) + $cargoArgs) }
        Assert-LocalPath $installer
        if (!(Test-Path -LiteralPath $installer -PathType Leaf) -or (Get-ProductVersion $installer) -ne $v) {
            throw 'Expected new version-matched x64 NSIS bundle was not produced.'
        }
        Assert-NsisBundle $installer
        Assert-LocalPath $app
        Assert-X64Application $app
        if ((Get-ProductVersion $app) -ne $v) { throw 'Built application and installer versions disagree.' }
        "InstallerPath: $installer"
        "Version: $v"
        "Bytes: $((Get-Item -LiteralPath $installer).Length)"
        "SHA256: $((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash)"
        "BuiltExeSHA256: $((Get-FileHash -LiteralPath $app -Algorithm SHA256).Hash)"
        'Installed payload SHA256 is not derived here; obtain a verified NSIS payload digest from the release coordinator. Do not use BuiltExeSHA256 as ExpectedExeSha256 for Install.'
        if ($WithMsix) { New-DarbotMsix $app $v (Get-FileHash -LiteralPath $app -Algorithm SHA256).Hash }
    } finally { Pop-Location }
}

function Get-InstalledVersion {
    Assert-LocalPath $installedExe
    $previous = [Microsoft.Win32.Registry]::GetValue('HKEY_CURRENT_USER\Software\darbotlm\darbot','','')
    if ($previous -and $previous -ine $installDir) { throw 'Non-default previous install location; manual review required.' }
    foreach ($key in 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\darbot',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\darbot') {
        if (Test-Path -LiteralPath $key) { throw 'Machine-wide Darbot installation present; manual review required.' }
    }
    if (!(Test-Path -LiteralPath $arpKey)) {
        if (Test-Path -LiteralPath $installedExe) { throw 'Unregistered Darbot executable; manual review required.' }
        return
    }
    $reg = Get-Item -LiteralPath $arpKey
    if ($reg.GetValue('DisplayName') -ine 'darbot' -or $reg.GetValue('Publisher') -ine 'darbotlm' -or
        $reg.GetValue('MainBinaryName') -ine 'darbot-desktop.exe' -or
        ([string]$reg.GetValue('InstallLocation')).Trim('"') -ine $installDir -or
        !(Test-Path -LiteralPath $installedExe -PathType Leaf)) { throw 'Unexpected or incomplete per-user Darbot installation.' }
    Assert-X64Application $installedExe
    $v = Get-ProductVersion $installedExe
    if ($v -ne $reg.GetValue('DisplayVersion')) { throw 'Installed PE and uninstall registration disagree.' }
    return $v
}

function Get-InstallAction([string]$Old, [string]$New, [bool]$SamePayload, [switch]$Replace) {
    if (!$Old) { return 'Install' }
    if ([version]$Old -gt [version]$New) { throw "Downgrade refused: $Old is already installed." }
    if ($Old -eq $New) {
        if ($SamePayload) { return 'Skip' }
        if (!$Replace) {
            throw "Version $Old exists with a different or unknown receipt. Use -ReplaceSameVersion only for an intentional replacement."
        }
    }
    return 'Update'
}

function Get-ToolboxReceipt {
    return [pscustomobject]@{
        Payload = [string][Microsoft.Win32.Registry]::GetValue($receipt,'VerifiedPayload','')
        Version = [string][Microsoft.Win32.Registry]::GetValue($receipt,'VerifiedVersion','')
    }
}

function Install-Darbot {
    Assert-StandardUser
    if ($ExpectedVersion -notmatch $stableVersion -or $ExpectedSha256 -notmatch '\A[0-9a-f]{64}\z' -or
        $ExpectedExeSha256 -notmatch '\A[0-9a-f]{64}\z' -or !$InstallerPath) {
        throw 'Install requires -InstallerPath, -ExpectedVersion X.Y.Z and trusted 64-digit -ExpectedSha256 and -ExpectedExeSha256.'
    }
    $null = [version]$ExpectedVersion
    $leaf = "darbot_${ExpectedVersion}_x64-setup.exe"
    if ($InstallerPath -match '^[/\\]{2}|::' -or $InstallerPath -match '^(?![a-z]:[/\\])[^/\\]+:') {
        throw 'Specify an ordinary local file, not a network, drive-relative or provider path.'
    }
    $file = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallerPath)
    Assert-LocalPath $file
    if ([IO.Path]::GetFileName($file) -ine $leaf -or !(Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Provide the original local installer named $leaf."
    }
    # Deny writes/replacement from hashing through process completion.
    $stream = [IO.File]::Open($file,'Open','Read','Read')
    try {
        $digest = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash
        if ($digest -ne $ExpectedSha256) { throw 'Installer SHA256 mismatch. Nothing executed.' }
        if ((Get-ProductVersion $file) -ne $ExpectedVersion) { throw 'Installer PE version differs from trusted release version.' }
        Assert-NsisBundle $file
        $old = Get-InstalledVersion
        $same = $false
        if ($old -eq $ExpectedVersion) {
            $appHash = (Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash
            $known = Get-ToolboxReceipt
            $same = $known.Payload -eq "${digest}:$appHash" -and $appHash -eq $ExpectedExeSha256 -and
                (!$known.Version -or $known.Version -eq $ExpectedVersion)
        }
        $action = Get-InstallAction $old $ExpectedVersion $same -Replace:$ReplaceSameVersion
        if ($action -eq 'Skip') { Write-Host "Already installed: verified $old payload."; return }
        if (!(Test-WebView)) { throw 'WebView2 missing. Run Prerequisites -For Runtime before Install.' }
        if (Test-Path -LiteralPath $installDir) {
            Assert-PlainTree ([IO.DirectoryInfo]$installDir)
            if (!$old -and @(Get-ChildItem -LiteralPath $installDir -Force | Select-Object -First 1).Count) {
                throw 'Nonempty unregistered install directory; manual review required.'
            }
        }
        $flags = if ($action -eq 'Update') { '/S /UPDATE' } else { '/S' }
        if ([Diagnostics.Process]::GetProcessesByName('darbot-desktop').Length) {
            throw 'Darbot is running. Save your work, exit including the tray, then retry; nothing was closed.'
        }
        Write-Host "Installing Darbot $ExpectedVersion ($flags). Keep Darbot closed."
        $proc = Start-Process -FilePath $file -ArgumentList $flags -WorkingDirectory ([Environment]::SystemDirectory) -PassThru -Wait
        try { Assert-ExitCode 'NSIS' $proc.ExitCode }
        finally { $proc.Dispose() }
        if ((Get-InstalledVersion) -ne $ExpectedVersion) { throw 'Installation did not produce the expected registered Darbot PE.' }
        $appStream = [IO.File]::Open($installedExe,'Open','Read','Read')
        try {
            $appHash = (Get-FileHash -InputStream $appStream -Algorithm SHA256).Hash
            if ($appHash -ne $ExpectedExeSha256) { throw 'Installed executable SHA256 differs from the trusted release payload; no receipt recorded.' }
            [Microsoft.Win32.Registry]::SetValue($receipt,'VerifiedVersion',$ExpectedVersion)
            [Microsoft.Win32.Registry]::SetValue($receipt,'VerifiedPayload',"${digest}:$appHash")
        } finally { $appStream.Dispose() }
        Write-Host "Installed and verified Darbot $ExpectedVersion."
    } finally { $stream.Dispose() }
}

function Assert-StandardUser {
    $user = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($user.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run Install/Launch in a non-elevated shell as the desktop user.'
    }
}

function Initialize-DesktopInterop {
    if ('DarbotDesktopToolbox.Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;

namespace DarbotDesktopToolbox {
    public static class Native {
        [DllImport("advapi32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CloseHandle(IntPtr handle);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder name, ref uint size);
        [DllImport("user32.dll")]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool IsWindowVisible(IntPtr window);
        [DllImport("user32.dll")]
        public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

        public static string OwnerSid(IntPtr process) {
            IntPtr token;
            if (!OpenProcessToken(process, 8, out token))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                using (var identity = new WindowsIdentity(token)) { return identity.User.Value; }
            } finally { CloseHandle(token); }
        }

        public static string ImagePath(IntPtr process) {
            uint size = 32768;
            var name = new StringBuilder((int)size);
            if (!QueryFullProcessImageName(process, 0, name, ref size))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            return name.ToString();
        }
    }
}
'@
}

function Assert-OwnedDarbot([Diagnostics.Process]$Process, [long]$StartTicks = 0) {
    # Retain the kernel process handle before querying identity. A recycled PID
    # must never redirect activation or window verification to a replacement.
    $handle = $Process.Handle
    if ($Process.HasExited) {
        Assert-ExitCode 'Darbot' $Process.ExitCode
        throw 'The owned Darbot instance exited; no replacement instance accepted.'
    }
    $ticks = $Process.StartTime.ToUniversalTime().Ticks
    $current = [Diagnostics.Process]::GetCurrentProcess()
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        if (($StartTicks -and $ticks -ne $StartTicks) -or $Process.SessionId -ne $current.SessionId -or
            [DarbotDesktopToolbox.Native]::ImagePath($handle) -ine $installedExe -or
            [DarbotDesktopToolbox.Native]::OwnerSid($handle) -ne $identity.User.Value) {
            throw 'Darbot process identity is not the installed executable owned by this user in this session.'
        }
    } finally {
        $identity.Dispose()
        $current.Dispose()
    }
    if ($Process.HasExited) {
        Assert-ExitCode 'Darbot' $Process.ExitCode
        throw 'The owned Darbot instance exited during identity verification.'
    }
    return $ticks
}

function Test-VisibleDarbotWindow([Diagnostics.Process]$Process) {
    $window = $Process.MainWindowHandle
    if ($window -eq [IntPtr]::Zero -or ![DarbotDesktopToolbox.Native]::IsWindowVisible($window)) { return $false }
    $windowProcessId = [uint32]0
    $thread = [DarbotDesktopToolbox.Native]::GetWindowThreadProcessId($window, [ref]$windowProcessId)
    return ($thread -ne 0 -and $windowProcessId -eq $Process.Id -and !$Process.HasExited)
}

function Assert-LaunchReceipt([string]$Version, [string]$AppHash) {
    $known = Get-ToolboxReceipt
    if ($known.Payload -notmatch '\A[0-9a-f]{64}:([0-9a-f]{64})\z' -or $Matches[1] -ne $AppHash -or
        ($known.Version -and $known.Version -ne $Version)) {
        throw 'Installed executable has no matching verified installer/payload receipt. Reverify with Install and trusted release hashes.'
    }
}

function Start-Darbot {
    Assert-StandardUser
    $v = Get-InstalledVersion
    if (!$v) { throw 'No default per-user Darbot installation found.' }
    if (!(Test-WebView)) { throw 'WebView2 missing. Run Prerequisites -For Runtime first.' }
    $stream = [IO.File]::Open($installedExe,'Open','Read','Read')
    $apps = @()
    $proc = $null
    $activation = $null
    try {
        Assert-LaunchReceipt $v (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash
        Initialize-DesktopInterop
        $apps = @([Diagnostics.Process]::GetProcessesByName('darbot-desktop'))
        if ($apps.Count) {
            if ($apps.Count -ne 1) { throw 'Multiple Darbot instances exist; no second instance started.' }
            $proc = $apps[0]
            $startTicks = Assert-OwnedDarbot $proc
            $outcome = 'Already running'
            $proc.Refresh()
            if (!(Test-VisibleDarbotWindow $proc)) {
                $null = Assert-OwnedDarbot $proc $startTicks
                # Only the verified installed executable may request the existing
                # single-instance callback; never substitute a newly discovered PID.
                $activation = Start-Process -FilePath $installedExe -WorkingDirectory $installDir -PassThru
                $outcome = 'Activated existing window'
            }
        } else {
            $proc = Start-Process -FilePath $installedExe -WorkingDirectory $installDir -PassThru
            $startTicks = Assert-OwnedDarbot $proc
            $outcome = 'Started'
        }
        $timer = [Diagnostics.Stopwatch]::StartNew()
        do {
            $proc.Refresh()
            $null = Assert-OwnedDarbot $proc $startTicks
            $activationComplete = !$activation
            if ($activation -and $activation.HasExited) {
                Assert-ExitCode 'Darbot single-instance activation' $activation.ExitCode
                $activationComplete = $true
            }
            if ($activationComplete -and (Test-VisibleDarbotWindow $proc) -and $proc.Responding) {
                $null = Assert-OwnedDarbot $proc $startTicks
                Write-Host "${outcome}: Darbot $v has a responsive visible window (PID $($proc.Id))."; return
            }
            Start-Sleep -Milliseconds 250
        } while ($timer.Elapsed.TotalSeconds -lt 20)
        throw 'No responsive visible window on the original owned Darbot instance and completed activation confirmed within 20s. No process was closed or restarted.'
    } finally {
        if ($activation) { $activation.Dispose() }
        foreach ($app in $apps) { $app.Dispose() }
        if (!$apps.Count -and $proc) { $proc.Dispose() }
        $stream.Dispose()
    }
}

$savedEnv = @{}
$lock = $null
$held = $false
$status = 0
try {
    $allowed = switch ($Mode) {
        Prerequisites { 'For' }
        Build { 'Offline','WithMsix','MsixOutputDirectory','MsixPublisher','AllowUnsignedMsix','MsixCertificateThumbprint','MsixTimestampUrl' }
        Package { 'Offline','WithMsix','MsixOutputDirectory','MsixPublisher','AllowUnsignedMsix','MsixCertificateThumbprint','MsixTimestampUrl' }
        Msix { 'ApplicationPath','ExpectedVersion','ExpectedExeSha256','MsixOutputDirectory','MsixPublisher','AllowUnsignedMsix','MsixCertificateThumbprint','MsixTimestampUrl' }
        Install { 'InstallerPath','ExpectedVersion','ExpectedSha256','ExpectedExeSha256','ReplaceSameVersion' }
    }
    foreach ($key in 'For','Offline','InstallerPath','ExpectedVersion','ExpectedSha256','ExpectedExeSha256','ReplaceSameVersion',
        'WithMsix','ApplicationPath','MsixOutputDirectory','MsixPublisher','AllowUnsignedMsix','MsixCertificateThumbprint','MsixTimestampUrl') {
        if ($PSBoundParameters.ContainsKey($key) -and $key -notin $allowed) { throw "-$key is not used by $Mode." }
        if ($Mode -in 'Build','Package' -and !$WithMsix -and
            $PSBoundParameters.ContainsKey($key) -and $key -in 'MsixOutputDirectory','MsixPublisher','AllowUnsignedMsix','MsixCertificateThumbprint','MsixTimestampUrl') {
            throw "-$key requires -WithMsix."
        }
    }
    if ($Mode -eq 'Help') { Get-Help -Name $PSCommandPath -Full; exit 0 }
    if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or ![Environment]::Is64BitProcess -or
        [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Environment',
            'PROCESSOR_ARCHITECTURE','') -ine 'AMD64') { throw 'Use 64-bit PowerShell on native Windows x64.' }
    if ($Mode -in 'Build','Package','Prerequisites') {
        foreach ($key in 'Path','CARGO_TARGET_DIR','CARGO_BUILD_BUILD_DIR','CARGO_NET_OFFLINE','COPILOT_SKIP_CLI_DOWNLOAD','TAURI_CONFIG',
            'RUSTUP_TOOLCHAIN','RUSTUP_AUTO_INSTALL') {
            $savedEnv[$key] = [Environment]::GetEnvironmentVariable($key)
        }
    }
    if ($Mode -in 'Install','Launch') {
        $installDir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'darbot'
        $installedExe = Join-Path $installDir 'darbot-desktop.exe'
        $lock = [Threading.Mutex]::new($false,'Local\Darbot.Desktop.Toolbox')
        try { $held = $lock.WaitOne(0) } catch [Threading.AbandonedMutexException] { $held = $true }
        if (!$held) { throw 'Another toolbox install/launch is in progress.' }
    }
    switch ($Mode) {
        Doctor { Show-Doctor }
        Prerequisites { Install-Prerequisites }
        Build { Invoke-Build }
        Package { Invoke-Build -PackageOnly }
        Msix { New-DarbotMsix $ApplicationPath $ExpectedVersion $ExpectedExeSha256 }
        Install { Install-Darbot }
        Launch { Start-Darbot }
    }
} catch {
    [Console]::Error.WriteLine("Darbot ${Mode}: $($_.Exception.Message)")
    $status = 1
    if ($_.Exception.Data.Contains('ExitCode')) { $status = [int]$_.Exception.Data['ExitCode'] }
} finally {
    if ($held) { $lock.ReleaseMutex() }
    if ($lock) { $lock.Dispose() }
    foreach ($key in $savedEnv.Keys) {
        if ($null -eq $savedEnv[$key]) { Remove-Item -LiteralPath "Env:\$key" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($key,$savedEnv[$key],'Process') }
    }
}
exit $status
