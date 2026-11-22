#requires -Version 5.1

<#
.SYNOPSIS
Build, install or launch the Windows x64 Darbot desktop release. No action by default.
.DESCRIPTION
Help and Doctor do not install anything. Doctor reports local presence, not sign-in,
provider compatibility or live connectivity. Use the provider's official consent flow.
Prerequisites defaults to Runtime (WebView2). Build tools are an explicit opt-in.
Prerequisites may download packages, accept their installer agreements and require UAC.
Build and Package run the checkout's code: use only a trusted, exclusively controlled checkout and
toolchain. It uses a new target on every run, never cargo clean or recursive deletion.
Keep the printed installer and remove old toolbox build directories yourself as needed.
-Offline restricts Cargo dependency resolution; Bun and Tauri's pinned bundler tools
can still need network access. No publishing, provider setup or live tests are requested.
Package produces the same optimized installer as Build, without running regression suites.
It does not mark a release as qualified or publish it.
Install needs no checkout or build tools. Supply the ORIGINAL darbot_X.Y.Z_x64-setup.exe
and the SHA256 from a trusted release channel, not a hash calculated from the download.
Close Darbot (including the tray) and keep it closed until installation finishes.
Install and Launch support the normal, non-elevated, default per-user installation only.
After verification, Install stores only the installer and installed-exe digests under
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
.\toolbox.ps1 Install -InstallerPath .\darbot_0.0.26_x64-setup.exe -ExpectedVersion 0.0.26 -ExpectedSha256 $trustedReleaseHash
.EXAMPLE
.\toolbox.ps1 Launch
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Position = 0)]
    [ValidateSet('Help','Doctor','Prerequisites','Build','Package','Install','Launch')]
    [string]$Mode = 'Help',
    [ValidateSet('Runtime','Build')] [string]$For = 'Runtime',
    [switch]$Offline,
    [string]$InstallerPath,
    [string]$ExpectedVersion,
    [string]$ExpectedSha256,
    [switch]$ReplaceSameVersion
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
    if ($LASTEXITCODE -ne 0) {
        throw "$([IO.Path]::GetFileName($Program)) $($Arguments[0]) failed (exit $LASTEXITCODE)."
    }
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
        if ($LASTEXITCODE -ne 0) { throw 'Visual Studio inventory failed.' }
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
                if ($proc.ExitCode -ne 0) { throw "VS Installer returned $($proc.ExitCode); finish setup/reboot if required." }
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

# Darbot's app and setup have the same VERSIONINFO. Distinguish the actual
# Tauri/NSIS self-extractor by its archive header just after the PE sections.
# This is a format check, not a substitute for an independently trusted hash.
function Assert-NsisBundle([string]$Path) {
    $r = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        $s = $r.BaseStream
        $s.Position = 60
        $pe = $r.ReadInt32()
        $s.Position = $pe
        if ($r.ReadUInt32() -ne 0x4550) { throw 'Missing PE signature.' }
        $s.Position = $pe + 6
        $count = $r.ReadUInt16()
        $s.Position = $pe + 20
        $sections = $pe + 24 + $r.ReadUInt16()
        $end = 0L
        for ($i = 0; $i -lt $count; $i++) {
            $s.Position = $sections + 40 * $i + 16
            $length = $r.ReadUInt32()
            $end = [Math]::Max($end, ([long]$r.ReadUInt32() + $length))
        }
        $s.Position = $end + 4
        if ($r.ReadUInt32() -ne 0xDEADBEEFL -or [Text.Encoding]::ASCII.GetString($r.ReadBytes(12)) -cne 'NullsoftInst') {
            throw 'Expected a Darbot NSIS setup, not an application executable.'
        }
    } finally { $r.Dispose() }
}

function Invoke-Build([switch]$PackageOnly) {
    # Never interpret an inherited target as a directory to clean. Reject
    # ambiguous configurations before installing dependencies or creating output.
    foreach ($key in 'CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR','CARGO_BUILD_DIR','CARGO_BUILD_BUILD_DIR',
        'CARGO_BUILD_TARGET','TAURI_CONFIG','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_BUILD_RUSTFLAGS',
        'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS') {
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
    $dist = Join-Path $desktop 'dist'
    Assert-LocalPath $dist
    if ([IO.Directory]::Exists($dist)) { Assert-PlainTree ([IO.DirectoryInfo]$dist) }
    Assert-LocalPath "$desktop\node_modules"
    $target = Join-Path $native ('target\tb-' + [guid]::NewGuid().ToString('N').Substring(0,12))
    Assert-LocalPath $target
    Update-ProcessPath
    Assert-BuildTools
    $bun = Find-Tool @('bun.exe','bun.cmd')
    $cargo = Find-Tool 'cargo.exe'
    $null = New-Item -Path $target -ItemType Directory -ErrorAction Stop
    # This target is new even if an earlier build of this exact version failed.
    $env:CARGO_TARGET_DIR = $target
    $env:CARGO_BUILD_BUILD_DIR = $target
    $cargoArgs = @('--locked')
    if ($Offline) { $cargoArgs += '--offline'; $env:CARGO_NET_OFFLINE = 'true' }
    $env:COPILOT_SKIP_CLI_DOWNLOAD = '1'
    $installer = Join-Path $target "$triple\release\bundle\nsis\darbot_${v}_x64-setup.exe"
    Push-Location -LiteralPath $desktop
    try {
        Invoke-Checked $bun @('install','--frozen-lockfile')
        foreach ($bin in 'tsc','tauri','vite') {
            if (!(Test-Path -LiteralPath "node_modules\.bin\$bin.exe") -and
                !(Test-Path -LiteralPath "node_modules\.bin\$bin.cmd") -and
                !(Test-Path -LiteralPath "node_modules\.bin\$bin")) { throw "Missing locked desktop binary: $bin." }
        }
        Invoke-Checked $bun @('run','--bun','--no-install','typecheck')
        if (!$PackageOnly) { Invoke-Checked $bun @('test','./src') }
        Invoke-Checked $bun (@('run','--bun','--no-install','tauri','build','--target',$triple,'--bundles','nsis','--ci','--') + $cargoArgs)
        Set-Location -LiteralPath $native
        # Candidate build.rs embeds the Common Controls manifest for MSVC.
        # Do not inject test-only linker flags into the release environment.
        if (!$PackageOnly) { Invoke-Checked $cargo (@('test','--lib','--bins','--target',$triple) + $cargoArgs) }
        Assert-LocalPath $installer
        if (!(Test-Path -LiteralPath $installer -PathType Leaf) -or (Get-ProductVersion $installer) -ne $v) {
            throw 'Expected new version-matched x64 NSIS bundle was not produced.'
        }
        Assert-NsisBundle $installer
        "InstallerPath: $installer"
        "Version: $v"
        "Bytes: $((Get-Item -LiteralPath $installer).Length)"
        "SHA256: $((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash)"
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

function Install-Darbot {
    if ($ExpectedVersion -notmatch $stableVersion -or $ExpectedSha256 -notmatch '\A[0-9a-f]{64}\z' -or !$InstallerPath) {
        throw 'Install requires -InstallerPath, -ExpectedVersion X.Y.Z and a trusted 64-digit -ExpectedSha256.'
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
            $known = [Microsoft.Win32.Registry]::GetValue($receipt,'VerifiedPayload','')
            $same = $known -eq "${digest}:$appHash"
        }
        $action = Get-InstallAction $old $ExpectedVersion $same -Replace:$ReplaceSameVersion
        if ($action -eq 'Skip') { Write-Host "Already installed: verified $old payload."; return }
        Assert-StandardUser
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
        if ($proc.ExitCode -ne 0) { throw "NSIS failed (exit $($proc.ExitCode))." }
        if ((Get-InstalledVersion) -ne $ExpectedVersion) { throw 'Installation did not produce the expected registered Darbot PE.' }
        $appHash = (Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash
        [Microsoft.Win32.Registry]::SetValue($receipt,'VerifiedPayload',"${digest}:$appHash")
        Write-Host "Installed and verified Darbot $ExpectedVersion."
    } finally { $stream.Dispose() }
}

function Assert-StandardUser {
    $user = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($user.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run Install/Launch in a non-elevated shell as the desktop user.'
    }
}

function Start-Darbot {
    Assert-StandardUser
    $v = Get-InstalledVersion
    if (!$v) { throw 'No default per-user Darbot installation found.' }
    if (!(Test-WebView)) { throw 'WebView2 missing. Run Prerequisites -For Runtime first.' }
    $apps = @([Diagnostics.Process]::GetProcessesByName('darbot-desktop'))
    if ($apps.Count) {
        if ($apps.Count -ne 1 -or $apps[0].SessionId -ne [Diagnostics.Process]::GetCurrentProcess().SessionId -or
            $apps[0].MainModule.FileName -ine $installedExe) { throw 'Another Darbot instance exists; no second instance started.' }
        $proc = $apps[0]
        $outcome = 'Already running'
        if ($proc.MainWindowHandle -eq [IntPtr]::Zero) {
            # Darbot handles a second launch by revealing its existing tray window.
            $activation = Start-Process -FilePath $installedExe -WorkingDirectory $installDir -PassThru
            $activation.Dispose()
            $outcome = 'Requested existing window'
        }
    } else {
        $proc = Start-Process -FilePath $installedExe -WorkingDirectory $installDir -PassThru
        $outcome = 'Started'
    }
    try {
        $timer = [Diagnostics.Stopwatch]::StartNew()
        do {
            $proc.Refresh()
            if ($proc.HasExited) { throw "Darbot exited before becoming responsive (exit $($proc.ExitCode))." }
            if ($proc.MainWindowHandle -ne [IntPtr]::Zero -and $proc.Responding) {
                Write-Host "${outcome}: Darbot $v has a responsive window."; return
            }
            Start-Sleep -Milliseconds 250
        } while ($timer.Elapsed.TotalSeconds -lt 20)
        throw 'No responsive Darbot window confirmed within 20s. Check the tray; no process was closed or restarted.'
    } finally { $proc.Dispose() }
}

$savedEnv = @{}
$lock = $null
$held = $false
$status = 0
try {
    $allowed = switch ($Mode) {
        Prerequisites { 'For' }
        Build { 'Offline' }
        Package { 'Offline' }
        Install { 'InstallerPath','ExpectedVersion','ExpectedSha256','ReplaceSameVersion' }
    }
    foreach ($key in 'For','Offline','InstallerPath','ExpectedVersion','ExpectedSha256','ReplaceSameVersion') {
        if ($PSBoundParameters.ContainsKey($key) -and $key -notin $allowed) { throw "-$key is not used by $Mode." }
    }
    if ($Mode -eq 'Help') { Get-Help -Name $PSCommandPath -Full; exit 0 }
    if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or ![Environment]::Is64BitProcess -or
        [Microsoft.Win32.Registry]::GetValue('HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Session Manager\Environment',
            'PROCESSOR_ARCHITECTURE','') -ine 'AMD64') { throw 'Use 64-bit PowerShell on native Windows x64.' }
    if ($Mode -in 'Build','Package','Prerequisites') {
        foreach ($key in 'Path','CARGO_TARGET_DIR','CARGO_BUILD_BUILD_DIR','CARGO_NET_OFFLINE','COPILOT_SKIP_CLI_DOWNLOAD',
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
        Install { Install-Darbot }
        Launch { Start-Darbot }
    }
} catch {
    [Console]::Error.WriteLine("Darbot ${Mode}: $($_.Exception.Message)")
    $status = 1
} finally {
    if ($held) { $lock.ReleaseMutex() }
    if ($lock) { $lock.Dispose() }
    foreach ($key in $savedEnv.Keys) {
        if ($null -eq $savedEnv[$key]) { Remove-Item -LiteralPath "Env:\$key" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($key,$savedEnv[$key],'Process') }
    }
}
exit $status
