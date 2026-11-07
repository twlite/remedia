[CmdletBinding()]
param(
  [switch]$NoFFmpeg,
  [string]$Version = $(if ($env:REMEDIA_VERSION) { $env:REMEDIA_VERSION } else { 'latest' }),
  [string]$InstallDir = $(if ($env:REMEDIA_INSTALL_DIR) { $env:REMEDIA_INSTALL_DIR } else { Join-Path $HOME '.remedia' })
)
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitOperatingSystem -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) {
  throw 'remedia-server currently publishes a Windows x64 MSVC binary only.'
}
$repo = 'twlite/remedia'
$headers = @{ 'User-Agent' = 'remedia-installer'; 'Accept' = 'application/vnd.github+json' }
if ($Version -eq 'latest') {
  $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers $headers
} else {
  if ($Version -notmatch '^v?\d+\.\d+\.\d+(-[a-zA-Z0-9.-]+)?$') { throw 'Invalid version.' }
  $tag = 'v' + $Version.TrimStart('v')
  $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/tags/$tag" -Headers $headers
}
$assetName = 'remedia-server-x86_64-pc-windows-msvc.tar.gz'
$asset = $release.assets | Where-Object name -eq $assetName | Select-Object -First 1
if (-not $asset -or $asset.digest -notmatch '^sha256:[a-fA-F0-9]{64}$') { throw "Missing release asset or checksum: $assetName" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("remedia-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $archive = Join-Path $tmp 'server.tar.gz'
  Invoke-WebRequest $asset.browser_download_url -OutFile $archive
  $actual = (Get-FileHash $archive -Algorithm SHA256).Hash
  if ($actual -ine $asset.digest.Substring(7)) { throw 'Release SHA-256 checksum mismatch.' }
  & tar -xzf $archive -C $tmp
  if ($LASTEXITCODE -ne 0) { throw 'Failed to extract server archive.' }
  $binDir = Join-Path $InstallDir 'bin'
  $libexec = Join-Path $InstallDir 'libexec'
  $licenseDir = Join-Path $InstallDir 'licenses'
  @($binDir, $libexec, $licenseDir) | ForEach-Object { New-Item -ItemType Directory -Path $_ -Force | Out-Null }
  $serverExe = Join-Path $tmp 'remedia-server-x86_64-pc-windows-msvc.exe'
  if (!(Test-Path $serverExe)) { throw 'Server binary not present in release archive.' }

  if (-not $NoFFmpeg) {
    $mamba = (Get-Command micromamba -ErrorAction SilentlyContinue).Source
    if (-not $mamba) {
      $mamba = Join-Path $binDir 'micromamba.exe'
      if (!(Test-Path $mamba)) {
        $archive = Join-Path $tmp 'micromamba.tar.bz2'
        Invoke-WebRequest 'https://micro.mamba.pm/api/micromamba/win-64/latest' -OutFile $archive
        & tar -xf $archive -C $tmp 'Library/bin/micromamba.exe'
        if ($LASTEXITCODE -ne 0) { throw 'Failed to extract micromamba executable.' }
        $downloaded = Join-Path $tmp 'Library/bin/micromamba.exe'
        if (!(Test-Path $downloaded)) { throw 'Unable to locate downloaded micromamba.exe.' }
        Copy-Item $downloaded $mamba
      }
    }
    $prefix = Join-Path $InstallDir 'ffmpeg9'
    $action = if (Test-Path (Join-Path $prefix 'conda-meta')) { 'install' } else { 'create' }
    & $mamba $action --yes --prefix $prefix --override-channels -c conda-forge 'ffmpeg=9.0.2'
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg 9 provisioning failed.' }
    if (!(Test-Path (Join-Path $InstallDir 'ffmpeg9\Library\bin\avcodec-63.dll'))) { throw 'FFmpeg 9 DLLs were not installed.' }
  }

  # Windows cannot replace a running executable. Place it in a versioned directory and
  # let the command wrapper discover the new version on the next invocation.
  $versionDir = Join-Path $libexec $release.tag_name
  New-Item -ItemType Directory -Path $versionDir -Force | Out-Null
  Copy-Item $serverExe (Join-Path $versionDir 'remedia-server.exe') -Force
  foreach ($name in @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt')) {
    Copy-Item (Join-Path $tmp $name) (Join-Path $licenseDir $name) -Force
  }
  Set-Content -LiteralPath (Join-Path $libexec 'current.txt') -Value $release.tag_name -NoNewline
  $wrapper = @'
@echo off
setlocal
set "REMEDIA_ROOT=%~dp0.."
if exist "%REMEDIA_ROOT%\ffmpeg9\Library\bin" set "PATH=%REMEDIA_ROOT%\ffmpeg9\Library\bin;%PATH%"
set /p REMEDIA_CURRENT=<"%REMEDIA_ROOT%\libexec\current.txt"
"%REMEDIA_ROOT%\libexec\%REMEDIA_CURRENT%\remedia-server.exe" %*
exit /b %ERRORLEVEL%
'@
  Set-Content -LiteralPath (Join-Path $binDir 'remedia.cmd') -Value $wrapper -Encoding Ascii
  Set-Content -LiteralPath (Join-Path $binDir 'remedia-server.cmd') -Value $wrapper -Encoding Ascii
  Write-Host "Installed Remedia $($release.tag_name) to $InstallDir"
  Write-Host "Add $binDir to PATH, then run: remedia --version"
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
