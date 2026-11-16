$ErrorActionPreference = 'Stop'
$vcpkgRoot = Join-Path $env:RUNNER_TEMP 'remedia-vcpkg'
# Classic mode uses the exact pinned overlay tree, so historical version Git objects are unnecessary.
if (!(Test-Path (Join-Path $vcpkgRoot 'ports/ffmpeg/portfile.cmake'))) {
  if ($env:REMEDIA_SOURCE_DIR) {
    node .github/scripts/source-checkout.mjs vcpkg $vcpkgRoot
  } else {
    git init $vcpkgRoot
    git -C $vcpkgRoot remote add origin https://github.com/microsoft/vcpkg.git
    git -C $vcpkgRoot fetch --depth=1 origin 14439b1e7cdaac4c6f466fee3deede319fcece8f
    if ($LASTEXITCODE -ne 0) { throw 'vcpkg pinned fetch failed' }
    git -C $vcpkgRoot checkout --detach FETCH_HEAD
  }
  if ($LASTEXITCODE -ne 0) { throw 'vcpkg pinned source setup failed' }
}
& (Join-Path $vcpkgRoot 'bootstrap-vcpkg.bat') -disableMetrics
if ($LASTEXITCODE -ne 0) { throw 'vcpkg bootstrap failed' }

$portFile = Join-Path $vcpkgRoot 'ports/ffmpeg/portfile.cmake'
$audioOptions = (Get-Content (Join-Path $env:GITHUB_WORKSPACE '.github/native-deps/ffmpeg-audio-options.txt')) -join ' '
$port = Get-Content -Raw $portFile
# Reset the single local edit before applying it on a rerun.
$port = $port.Replace($audioOptions + ' --enable-pic', '--enable-pic')
$port = $port.Replace('set(OPTIONS "--enable-pic', ('set(OPTIONS "' + $audioOptions + ' --enable-pic'))
[IO.File]::WriteAllText($portFile, $port, [Text.UTF8Encoding]::new($false))
$triplet = if ($env:RUST_TARGET -eq 'i686-pc-windows-msvc') { 'x86-windows-static-md' } elseif ($env:RUST_TARGET -eq 'aarch64-pc-windows-msvc') { 'arm64-windows-static-md' } else { 'x64-windows-static-md' }
# vcpkg-rs 0.2.15 resolves metadata under VCPKG_ROOT/installed.
$installRoot = Join-Path $vcpkgRoot 'installed'
$manifestRoot = Join-Path $env:GITHUB_WORKSPACE '.github\native-deps\windows-cross'
$env:VCPKG_ROOT = $vcpkgRoot
$env:VCPKG_DEFAULT_TRIPLET = $triplet
Push-Location $manifestRoot
try {
  $manifest = Get-Content -Raw (Join-Path $manifestRoot 'vcpkg.json') | ConvertFrom-Json
  $portManifest = Get-Content -Raw (Join-Path $vcpkgRoot 'ports/ffmpeg/vcpkg.json') | ConvertFrom-Json
  $pin = $manifest.overrides | Where-Object name -eq 'ffmpeg'
  if ($portManifest.version -ne $pin.version -or $portManifest.'port-version' -ne $pin.'port-version') { throw 'Pinned FFmpeg port version mismatch' }
  $features = ($manifest.dependencies | Where-Object name -eq 'ffmpeg').features -join ','
  if ($env:REMEDIA_SOURCE_DIR) {
    $downloads = Join-Path $vcpkgRoot 'downloads'
    New-Item -ItemType Directory -Force $downloads | Out-Null
    Copy-Item (Join-Path $env:REMEDIA_SOURCE_DIR 'ffmpeg.tar.gz') (Join-Path $downloads "ffmpeg-ffmpeg-n$($pin.version).tar.gz")
  }
  & (Join-Path $vcpkgRoot 'vcpkg.exe') install "ffmpeg[core,$features]" --classic --triplet $triplet --overlay-ports="$vcpkgRoot/ports" --x-install-root=$installRoot
  if ($LASTEXITCODE -ne 0) { throw "vcpkg FFmpeg install failed for $triplet" }
} finally {
  Pop-Location
}
"VCPKG_ROOT=$vcpkgRoot" >> $env:GITHUB_ENV
"VCPKG_INSTALL_ROOT=$installRoot" >> $env:GITHUB_ENV
"VCPKG_TARGET_TRIPLET=$triplet" >> $env:GITHUB_ENV
"VCPKGRS_TRIPLET=$triplet" >> $env:GITHUB_ENV
# Git for Windows also provides a link.exe (GNU coreutils). Pin
# Cargo's linker to MSVC instead of relying on PATH resolution.
$msvcTarget = if ($triplet -like 'x86-*') { 'x86' } elseif ($triplet -like 'arm64-*') { 'arm64' } else { 'x64' }
$msvcHost = if ($env:VSCMD_ARG_HOST_ARCH) { $env:VSCMD_ARG_HOST_ARCH } elseif ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
$msvcLinker = Join-Path $env:VCToolsInstallDir "bin/Host$msvcHost/$msvcTarget/link.exe"
if (!(Test-Path -LiteralPath $msvcLinker)) { throw "MSVC linker not found: $msvcLinker" }
$rustTriple = $env:RUST_TARGET.ToUpper().Replace('-', '_')
"CARGO_TARGET_${rustTriple}_LINKER=$msvcLinker" >> $env:GITHUB_ENV

$records = Join-Path $installRoot "$triplet/share/remedia-build"
if (!(Test-Path (Join-Path $records 'config.h'))) {
  New-Item -ItemType Directory -Force $records | Out-Null
  $configuration = Get-ChildItem (Join-Path $vcpkgRoot 'buildtrees/ffmpeg') -Filter config.h -Recurse | Where-Object { $_.FullName -match '-rel[\\/]' } | Select-Object -First 1
  if (!$configuration) { throw 'Missing release FFmpeg configuration' }
  Copy-Item $configuration.FullName (Join-Path $records 'config.h')
  Copy-Item (Join-Path $configuration.DirectoryName 'ffbuild/config.mak') (Join-Path $records 'config.mak')
}
$config = Get-Content -Raw (Join-Path $records 'config.h')
foreach ($required in @('CONFIG_GPL 1', 'CONFIG_VERSION3 1', 'CONFIG_NONFREE 0', 'CONFIG_STATIC 1', 'CONFIG_SHARED 0')) {
  if (!$config.Contains("#define $required")) { throw "Missing static FFmpeg policy: $required" }
}
if ($config -match '(?m)^#define CONFIG_LIB[A-Z0-9_]+ 1$') { throw 'Unaudited external FFmpeg dependency' }
