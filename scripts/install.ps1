[CmdletBinding()]
param(
  [switch]$NoFFmpeg,
  [switch]$FFmpegOnly,
  [string]$Version = $(if ($env:REMEDIA_VERSION) { $env:REMEDIA_VERSION } else { 'latest' }),
  [string]$InstallDir = $(if ($env:REMEDIA_INSTALL_DIR) { $env:REMEDIA_INSTALL_DIR } else { Join-Path $HOME '.remedia' }),
  [string]$ArchivePath = $env:REMEDIA_ARCHIVE_PATH
)
$ErrorActionPreference = 'Stop'
if ($FFmpegOnly -and $NoFFmpeg) {
  throw 'Conflicting options: -FFmpegOnly installs only FFmpeg and -NoFFmpeg installs only the server, so nothing would be left to install. Remove one of them.'
}
if (-not [Environment]::Is64BitOperatingSystem -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) {
  throw 'The Remedia installer currently supports Windows x64 only (remedia-server publishes a Windows x64 MSVC binary only).'
}
# FFmpeg 9 shared-library ABI majors (not application versions). Keep in sync with
# .github/scripts/ffmpeg-abi.mjs and packages/remedia-node/lib/ffmpeg-runtime.ts.
$ffmpegAbi = [ordered]@{ avcodec = 63; avformat = 63; avutil = 61; avfilter = 12; swresample = 7 }
$ffmpegPackage = 'ffmpeg=9.0.2'
$repo = 'twlite/remedia'
$headers = @{ 'User-Agent' = 'remedia-installer'; 'Accept' = 'application/vnd.github+json' }

function Write-InstallStatus {
  param([string]$Message)
  Write-Host "[remedia] $Message"
}

function Test-InstallPathEntry {
  param([string]$PathList, [string]$Directory)
  foreach ($entry in ($PathList -split ';')) {
    $expanded = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"'))
    $normalized = $expanded.TrimEnd([char[]]@('\', '/'))
    if ([string]::Equals($normalized, $Directory, [StringComparison]::OrdinalIgnoreCase)) {
      return $true
    }
  }
  return $false
}

function Add-RemediaToPath {
  param([string]$Directory)
  $fullDirectory = [IO.Path]::GetFullPath($Directory).TrimEnd([char[]]@('\', '/'))
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not (Test-InstallPathEntry -PathList $userPath -Directory $fullDirectory)) {
    $separator = if ([string]::IsNullOrEmpty($userPath) -or $userPath.EndsWith(';')) { '' } else { ';' }
    [Environment]::SetEnvironmentVariable('Path', "$userPath$separator$fullDirectory", 'User')
    Write-InstallStatus "Added $fullDirectory to the user PATH."
  } else {
    Write-InstallStatus 'Remedia is already in the user PATH.'
  }
  if (-not (Test-InstallPathEntry -PathList $env:Path -Directory $fullDirectory)) {
    $env:Path = "$fullDirectory;$env:Path"
  }
}

function Show-DownloadLine {
  param([string]$Message, [ref]$PreviousWidth)
  $width = [Math]::Max($PreviousWidth.Value, $Message.Length)
  Write-Host ([char]13 + '  ' + $Message.PadRight($width)) -NoNewline
  $PreviousWidth.Value = $width
}

function Format-DownloadProgress {
  param([long]$Received, [long]$Total, [double]$ElapsedSeconds)
  $speed = ($Received / 1MB) / [Math]::Max(0.1, $ElapsedSeconds)
  if ($Total -le 0) {
    return ('{0:N1} MiB downloaded  {1:N1} MiB/s' -f ($Received / 1MB), $speed)
  }
  $percent = [int][Math]::Min(100, [Math]::Floor(100.0 * $Received / $Total))
  $filled = [int][Math]::Floor($percent / 5)
  $bar = '[' + ('#' * $filled) + ('.' * (20 - $filled)) + ']'
  return ('{0} {1,3}%  {2:N1}/{3:N1} MiB  {4:N1} MiB/s' -f $bar, $percent, ($Received / 1MB), ($Total / 1MB), $speed)
}

function Save-InstallDownload {
  param(
    [string]$Url,
    [string]$Destination,
    [string]$Label,
    [long]$ExpectedBytes = 0,
    [int]$Attempts = 4
  )

  # Write-Host works even when PowerShell's progress renderer is hidden by a host or preference.
  $interactive = -not [Console]::IsOutputRedirected
  $client = [System.Net.Http.HttpClient]::new()
  $client.DefaultRequestHeaders.UserAgent.ParseAdd('remedia-installer')
  $client.DefaultRequestHeaders.Accept.ParseAdd('application/octet-stream')
  $client.Timeout = [TimeSpan]::FromSeconds(60)

  try {
    for ($attempt = 1; $attempt -le $attempts; $attempt++) {
      Write-InstallStatus ("Downloading {0} (attempt {1}/{2})..." -f $Label, $attempt, $attempts)
      $clock = [Diagnostics.Stopwatch]::StartNew()
      $nextFeedback = 0
      $lineWidth = 0
      $activeLine = $false
      $response = $null
      $source = $null
      $destinationStream = $null
      $cancellation = [Threading.CancellationTokenSource]::new()

      try {
        # The connection can stall or return 504 before sending any bytes.
        $request = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead)
        while (-not $request.IsCompleted) {
          $elapsed = $clock.ElapsedMilliseconds
          if ($elapsed -ge $nextFeedback) {
            $message = 'Connecting to download server... {0}s elapsed' -f [int]$clock.Elapsed.TotalSeconds
            if ($interactive) {
              Show-DownloadLine -Message $message -PreviousWidth ([ref]$lineWidth)
              $activeLine = $true
            } else {
              Write-Host "  $message"
            }
            $nextFeedback = $elapsed + $(if ($interactive) { 750 } else { 5000 })
          }
          Start-Sleep -Milliseconds 200
        }
        $response = $request.GetAwaiter().GetResult()
        $response.EnsureSuccessStatusCode() | Out-Null
        if ($activeLine) { Write-Host ''; $activeLine = $false }

        [long]$total = 0
        if ($null -ne $response.Content.Headers.ContentLength) {
          $total = [long]$response.Content.Headers.ContentLength
        } elseif ($ExpectedBytes -gt 0) {
          $total = $ExpectedBytes
        }

        $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $destinationStream = [IO.File]::Create($Destination)
        $buffer = New-Object byte[] (128KB)
        [long]$received = 0
        $lastByteAt = $clock.ElapsedMilliseconds
        $nextFeedback = 0

        while ($true) {
          $reading = $source.ReadAsync($buffer, 0, $buffer.Length, $cancellation.Token)
          while (-not $reading.IsCompleted) {
            if ($clock.ElapsedMilliseconds - $lastByteAt -ge 60000) {
              $cancellation.Cancel()
              throw [TimeoutException]::new('No download data received for 60 seconds.')
            }
            $elapsed = $clock.ElapsedMilliseconds
            if ($elapsed -ge $nextFeedback) {
              $message = Format-DownloadProgress -Received $received -Total $total -ElapsedSeconds $clock.Elapsed.TotalSeconds
              if ($interactive) {
                Show-DownloadLine -Message $message -PreviousWidth ([ref]$lineWidth)
                $activeLine = $true
              } else {
                Write-Host "  $message"
              }
              $nextFeedback = $elapsed + $(if ($interactive) { 750 } else { 5000 })
            }
            Start-Sleep -Milliseconds 200
          }

          $count = $reading.GetAwaiter().GetResult()
          if ($count -eq 0) { break }
          $destinationStream.Write($buffer, 0, $count)
          $received += $count
          $lastByteAt = $clock.ElapsedMilliseconds
          $elapsed = $clock.ElapsedMilliseconds
          if ($elapsed -ge $nextFeedback) {
            $message = Format-DownloadProgress -Received $received -Total $total -ElapsedSeconds $clock.Elapsed.TotalSeconds
            if ($interactive) {
              Show-DownloadLine -Message $message -PreviousWidth ([ref]$lineWidth)
              $activeLine = $true
            } else {
              Write-Host "  $message"
            }
            $nextFeedback = $elapsed + $(if ($interactive) { 750 } else { 5000 })
          }
        }

        if ($interactive) {
          $message = Format-DownloadProgress -Received $received -Total $total -ElapsedSeconds $clock.Elapsed.TotalSeconds
          Show-DownloadLine -Message $message -PreviousWidth ([ref]$lineWidth)
          $activeLine = $true
        }
        if ($activeLine) { Write-Host ''; $activeLine = $false }
        Write-InstallStatus ('Downloaded {0} ({1:N1} MiB)' -f $Label, ($received / 1MB))
        return
      } catch {
        if ($activeLine) { Write-Host ''; $activeLine = $false }
        $statusCode = if ($response) { [int]$response.StatusCode } else { 0 }
        $reason = if ($statusCode) { "HTTP $statusCode ($($response.ReasonPhrase))" } else { $_.Exception.Message }
        $transientError = $_.Exception -is [System.Net.Http.HttpRequestException] -or
          $_.Exception -is [System.Threading.Tasks.TaskCanceledException] -or
          $_.Exception -is [TimeoutException]
        $retryable = $transientError -or $statusCode -in @(408, 429, 500, 502, 503, 504)
        if (-not $retryable -or $attempt -eq $attempts) {
          throw "Failed to download $Label (attempt $attempt/$attempts): $reason"
        }
        $delay = [int][Math]::Pow(2, $attempt)
        Write-InstallStatus ("Download failed: {0}. Retrying in {1}s..." -f $reason, $delay)
      } finally {
        if ($destinationStream) { $destinationStream.Dispose() }
        if ($source) { $source.Dispose() }
        if ($response) { $response.Dispose() }
        $cancellation.Dispose()
      }

      if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Force -ErrorAction SilentlyContinue
      }
      Start-Sleep -Seconds $delay
    }
  } finally {
    $client.Dispose()
  }
}

# Resolve micromamba. A micromamba on PATH or in the install directory is reused. A missing one is
# downloaded to $BinaryDirectory, which is the server bin directory for server installs and a
# temporary directory for -FFmpegOnly so that no server directories are created.
function Get-RemediaMicromamba {
  param([string]$BinaryDirectory, [string]$WorkDirectory)
  $existing = (Get-Command micromamba -ErrorAction SilentlyContinue).Source
  if ($existing) { return $existing }
  $installed = Join-Path $InstallDir 'bin\micromamba.exe'
  if (Test-Path -LiteralPath $installed) { return $installed }
  $mamba = Join-Path $BinaryDirectory 'micromamba.exe'
  New-Item -ItemType Directory -Path $BinaryDirectory -Force | Out-Null
  $mambaArchive = Join-Path $WorkDirectory 'micromamba.tar.bz2'
  Save-InstallDownload -Url 'https://micro.mamba.pm/api/micromamba/win-64/latest' -Destination $mambaArchive -Label 'micromamba'
  Write-InstallStatus 'Extracting micromamba...'
  & tar -xf $mambaArchive -C $WorkDirectory 'Library/bin/micromamba.exe' | Out-Host
  if ($LASTEXITCODE -ne 0) { throw 'Failed to extract micromamba executable.' }
  $downloaded = Join-Path $WorkDirectory 'Library/bin/micromamba.exe'
  if (!(Test-Path -LiteralPath $downloaded)) { throw 'Unable to locate downloaded micromamba.exe.' }
  Copy-Item -LiteralPath $downloaded -Destination $mamba
  return $mamba
}

# Check that every FFmpeg shared library at the required ABI major exists in the prefix.
function Test-RemediaFFmpeg {
  param([string]$Prefix)
  $libraryDirectory = Join-Path $Prefix 'Library\bin'
  foreach ($entry in $ffmpegAbi.GetEnumerator()) {
    $dll = Join-Path $libraryDirectory ('{0}-{1}.dll' -f $entry.Key, $entry.Value)
    if (!(Test-Path -LiteralPath $dll)) { throw "FFmpeg 9 DLL was not installed: $dll" }
  }
  Write-InstallStatus ("Verified FFmpeg 9 libraries ({0}) in {1}" -f (($ffmpegAbi.GetEnumerator() | ForEach-Object { '{0}-{1}' -f $_.Key, $_.Value }) -join ', '), $libraryDirectory)
}

# Install or update FFmpeg 9 and its shared libraries in <install dir>\ffmpeg9.
function Install-RemediaFFmpeg {
  param([string]$BinaryDirectory, [string]$WorkDirectory)
  Write-InstallStatus 'Preparing FFmpeg 9 dependencies...'
  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  $mamba = Get-RemediaMicromamba -BinaryDirectory $BinaryDirectory -WorkDirectory $WorkDirectory
  $prefix = Join-Path $InstallDir 'ffmpeg9'
  $action = if (Test-Path -LiteralPath (Join-Path $prefix 'conda-meta')) { 'install' } else { 'create' }
  Write-InstallStatus 'Provisioning FFmpeg 9 via micromamba (package resolution may take a while)...'
  # Native command output must reach the console, not this function's return value (the prefix).
  & $mamba $action --yes --prefix $prefix --override-channels -c conda-forge $ffmpegPackage | Out-Host
  if ($LASTEXITCODE -ne 0) { throw 'FFmpeg 9 provisioning failed.' }
  Test-RemediaFFmpeg -Prefix $prefix
  return $prefix
}

# -FFmpegOnly never contacts GitHub releases and never touches server files.
if ($FFmpegOnly) {
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("remedia-" + [guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp -Force | Out-Null
  try {
    $ffmpegPrefix = Install-RemediaFFmpeg -BinaryDirectory (Join-Path $tmp 'bin') -WorkDirectory $tmp
    Write-Host "FFmpeg 9 is installed in $ffmpegPrefix"
    Write-Host '@remedia/node finds this installation automatically. Set REMEDIA_FFMPEG_DIR to use another prefix.'
  } finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
  return
}

Write-InstallStatus "Checking GitHub releases..."
if ($Version -eq 'latest') {
  # GitHub's latest endpoint excludes prereleases. Fall back only when no stable release exists.
  try {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers $headers
  } catch {
    if ([int]$_.Exception.Response.StatusCode -ne 404) { throw }
    $releases = @(Invoke-RestMethod "https://api.github.com/repos/$repo/releases?per_page=100" -Headers $headers)
    $release = $releases | Where-Object { -not $_.draft -and $_.prerelease } | Select-Object -First 1
    if (-not $release) { throw 'No published Remedia release found.' }
  }
} else {
  if ($Version -notmatch '^v?\d+\.\d+\.\d+(-[a-zA-Z0-9.-]+)?$') { throw 'Invalid version.' }
  $tag = 'v' + $Version.TrimStart('v')
  $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/tags/$tag" -Headers $headers
}
Write-InstallStatus "Selected Remedia $($release.tag_name) for Windows x64."
$assetName = 'remedia-server-x86_64-pc-windows-msvc.tar.gz'
$asset = $release.assets | Where-Object name -eq $assetName | Select-Object -First 1
if (-not $asset -or $asset.digest -notmatch '^sha256:[a-fA-F0-9]{64}$') { throw "Missing release asset or checksum: $assetName" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("remedia-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $archive = Join-Path $tmp 'server.tar.gz'
  if ($ArchivePath) {
    # A browser-downloaded release archive can bypass a failing CLI download route.
    $localArchive = (Resolve-Path -LiteralPath $ArchivePath -ErrorAction Stop).ProviderPath
    if (-not (Test-Path -LiteralPath $localArchive -PathType Leaf)) {
      throw "Release archive is not a file: $ArchivePath"
    }
    Copy-Item -LiteralPath $localArchive -Destination $archive
    Write-InstallStatus "Using locally downloaded release archive: $localArchive"
  } else {
    try {
      Save-InstallDownload -Url $asset.browser_download_url -Destination $archive -Label 'Remedia server' -ExpectedBytes $asset.size -Attempts 2
    } catch {
      if (-not $asset.url) { throw }
      Write-InstallStatus 'Direct GitHub download failed. Trying the release-asset API...'
      Save-InstallDownload -Url $asset.url -Destination $archive -Label 'Remedia server (API)' -ExpectedBytes $asset.size -Attempts 2
    }
  }
  Write-InstallStatus 'Verifying server archive SHA-256...'
  $actual = (Get-FileHash $archive -Algorithm SHA256).Hash
  if ($actual -ine $asset.digest.Substring(7)) { throw 'Release SHA-256 checksum mismatch.' }
  Write-InstallStatus 'Extracting server archive...'
  & tar -xzf $archive -C $tmp
  if ($LASTEXITCODE -ne 0) { throw 'Failed to extract server archive.' }
  $binDir = Join-Path $InstallDir 'bin'
  $libexec = Join-Path $InstallDir 'libexec'
  $licenseDir = Join-Path $InstallDir 'licenses'
  @($binDir, $libexec, $licenseDir) | ForEach-Object { New-Item -ItemType Directory -Path $_ -Force | Out-Null }
  $serverExe = Join-Path $tmp 'remedia-server-x86_64-pc-windows-msvc.exe'
  $runtimeExe = Join-Path $tmp 'remedia-server-runtime-x86_64-pc-windows-msvc.exe'
  if (!(Test-Path $serverExe) -or !(Test-Path $runtimeExe)) { throw 'CLI or runtime binary not present in release archive.' }

  if (-not $NoFFmpeg) {
    Install-RemediaFFmpeg -BinaryDirectory $binDir -WorkDirectory $tmp | Out-Null
  } else {
    Write-InstallStatus 'Skipping FFmpeg installation (-NoFFmpeg).'
  }

  Write-InstallStatus 'Installing Remedia binaries and commands...'
  # Windows cannot replace a running executable. Place it in a versioned directory and
  # let the command wrapper discover the new version on the next invocation.
  $versionDir = Join-Path $libexec $release.tag_name
  New-Item -ItemType Directory -Path $versionDir -Force | Out-Null
  Copy-Item $serverExe (Join-Path $versionDir 'remedia-server.exe') -Force
  Copy-Item $runtimeExe (Join-Path $versionDir 'remedia-server-runtime.exe') -Force
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
  Add-RemediaToPath -Directory $binDir
  Write-Host "Installed Remedia $($release.tag_name) to $InstallDir"
  Write-Host 'Run: remedia --version'
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
