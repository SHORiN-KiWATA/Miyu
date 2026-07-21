[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$UseRsProxy,
    [string]$RipgrepPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$distRoot = Join-Path $projectRoot 'dist'
$targetRoot = Join-Path $projectRoot 'target'

if (-not (Get-Command rustc -ErrorAction SilentlyContinue) -or
    -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Rust is not installed. Install it from https://rustup.rs and run this script again.'
}

if ([string]::IsNullOrWhiteSpace($RipgrepPath)) {
    $ripgrepCommand = Get-Command rg.exe -ErrorAction SilentlyContinue
    if ($null -ne $ripgrepCommand) {
        $RipgrepPath = $ripgrepCommand.Source
    }
}
if ([string]::IsNullOrWhiteSpace($RipgrepPath) -or
    -not (Test-Path -LiteralPath $RipgrepPath -PathType Leaf)) {
    throw @'
ripgrep (rg.exe) was not found. It is required by Miyu's glob/grep tools and their tests.
Install it, reopen PowerShell, and run this script again:

    winget install --id BurntSushi.ripgrep.MSVC -e

Alternatively, pass its full path with -RipgrepPath C:\path\to\rg.exe.
'@
}
$RipgrepPath = (Resolve-Path -LiteralPath $RipgrepPath).Path
& $RipgrepPath --version | Select-Object -First 1 | Write-Host
if ($LASTEXITCODE -ne 0) {
    throw "ripgrep could not be started: $RipgrepPath"
}

$rustFlagSeparator = [char]0x1f
$encodedRustFlags = @()
if (-not [string]::IsNullOrWhiteSpace($env:CARGO_ENCODED_RUSTFLAGS)) {
    $encodedRustFlags += $env:CARGO_ENCODED_RUSTFLAGS.Split($rustFlagSeparator)
}
elseif (-not [string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
    $encodedRustFlags += $env:RUSTFLAGS -split '\s+' | Where-Object { $_ }
}
$encodedRustFlags += "--remap-path-prefix=$projectRoot=PROJECT_ROOT"
if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
    $encodedRustFlags += "--remap-path-prefix=$env:USERPROFILE=USERPROFILE"
}
$rustHost = (& rustc -vV | Select-String '^host:').ToString().Substring(5).Trim()

if ($rustHost -like '*windows-gnu*') {
    if (-not (Get-Command x86_64-w64-mingw32-gcc -ErrorAction SilentlyContinue)) {
        throw 'The GNU Rust target needs x86_64-w64-mingw32-gcc. Install MSYS2/MinGW-w64 or switch Rust to the MSVC toolchain.'
    }
    if ($projectRoot -match '[^\x00-\x7F]') {
        $targetRoot = Join-Path $env:PUBLIC 'MiyuBuild\target'
        Write-Host "Using an ASCII-only build cache for MinGW: $targetRoot"
    }
    $rustSysroot = (& rustc --print sysroot).Trim()
    $lld = Join-Path $rustSysroot "lib\rustlib\$rustHost\bin\rust-lld.exe"
    if (Test-Path -LiteralPath $lld) {
        $encodedRustFlags += '-C', "linker=$lld", '-C', 'linker-flavor=ld.lld'
        Write-Host "Using Rust's bundled LLD linker: $lld"
    }
}

Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
$env:CARGO_ENCODED_RUSTFLAGS = $encodedRustFlags -join $rustFlagSeparator

$env:CARGO_TARGET_DIR = $targetRoot
$cargoOptions = @()
if ($UseRsProxy) {
    $cargoOptions += @(
        '--config', 'source.crates-io.replace-with=\"rsproxy\"',
        '--config', 'source.rsproxy.registry=\"sparse+https://rsproxy.cn/index/\"'
    )
    Write-Host 'Using rsproxy.cn for Rust dependencies.'
}
Push-Location $projectRoot
try {
    if (-not $SkipTests) {
        & cargo @cargoOptions test --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo test failed with exit code $LASTEXITCODE"
        }
    }

    & cargo @cargoOptions build --release --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    New-Item -ItemType Directory -Force -Path $distRoot | Out-Null
    Copy-Item -Force (Join-Path $targetRoot 'release\miyu.exe') (Join-Path $distRoot 'miyu.exe')
    Copy-Item -Force -LiteralPath $RipgrepPath -Destination (Join-Path $distRoot 'rg.exe')
    Copy-Item -Force (Join-Path $projectRoot 'README_WINDOWS.md') (Join-Path $distRoot 'README_WINDOWS.md')
    $localizedReadme = Get-ChildItem -LiteralPath $projectRoot -Filter 'README_*.md' |
        Where-Object { $_.Name -ne 'README_WINDOWS.md' } |
        Select-Object -First 1
    if ($null -ne $localizedReadme) {
        Copy-Item -Force -LiteralPath $localizedReadme.FullName `
            -Destination (Join-Path $distRoot $localizedReadme.Name)
    }
    Copy-Item -Force (Join-Path $projectRoot 'LICENSE') (Join-Path $distRoot 'LICENSE')

    $shareRoot = Join-Path $distRoot 'share\miyu'
    $defaultKbRoot = Join-Path $shareRoot 'default-kb'
    $memesRoot = Join-Path $shareRoot 'memes\miyu'
    New-Item -ItemType Directory -Force -Path $defaultKbRoot, $memesRoot | Out-Null
    Copy-Item -Recurse -Force (Join-Path $projectRoot 'kb\*') $defaultKbRoot
    Copy-Item -Recurse -Force (Join-Path $projectRoot 'src\memes\miyu\*') $memesRoot

    @'
@echo off
"%~dp0miyu.exe" %*
'@ | Set-Content -Encoding ASCII (Join-Path $distRoot 'miyu.cmd')

    & (Join-Path $distRoot 'miyu.exe') --version
    if ($LASTEXITCODE -ne 0) {
        throw 'The packaged executable did not pass its version smoke test.'
    }
    $archivePath = Join-Path $projectRoot 'Miyu-windows-x86_64.zip'
    Compress-Archive -Path (Join-Path $distRoot '*') -DestinationPath $archivePath -Force
    Write-Host "Windows package created at: $distRoot"
    Write-Host "Windows archive created at: $archivePath"
}
finally {
    Pop-Location
}
