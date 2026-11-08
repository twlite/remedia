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

function Write-InstallStatus {
  param([string]$Message)
  Write-Host "[remedia] $Message"
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
    [long]$ExpectedBytes = 0
  )

  # Write-Host works even when PowerShell's progress renderer is hidden by a host or preference.
  $interactive = -not [Console]::IsOutputRedirected
  $client = [System.Net.Http.HttpClient]::new()
  $client.DefaultRequestHeaders.UserAgent.ParseAdd('remedia-installer')
  $client.Timeout = [TimeSpan]::FromSeconds(60)
  $attempts = 4

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
  Save-InstallDownload -Url $asset.browser_download_url -Destination $archive -Label 'Remedia server' -ExpectedBytes $asset.size
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
    Write-InstallStatus 'Preparing FFmpeg 9 dependencies...'
    $mamba = (Get-Command micromamba -ErrorAction SilentlyContinue).Source
    if (-not $mamba) {
      $mamba = Join-Path $binDir 'micromamba.exe'
      if (!(Test-Path $mamba)) {
        $archive = Join-Path $tmp 'micromamba.tar.bz2'
        Save-InstallDownload -Url 'https://micro.mamba.pm/api/micromamba/win-64/latest' -Destination $archive -Label 'micromamba'
        Write-InstallStatus 'Extracting micromamba...'
        & tar -xf $archive -C $tmp 'Library/bin/micromamba.exe'
        if ($LASTEXITCODE -ne 0) { throw 'Failed to extract micromamba executable.' }
        $downloaded = Join-Path $tmp 'Library/bin/micromamba.exe'
        if (!(Test-Path $downloaded)) { throw 'Unable to locate downloaded micromamba.exe.' }
        Copy-Item $downloaded $mamba
      }
    }
    $prefix = Join-Path $InstallDir 'ffmpeg9'
    $action = if (Test-Path (Join-Path $prefix 'conda-meta')) { 'install' } else { 'create' }
    Write-InstallStatus 'Provisioning FFmpeg 9 via micromamba (package resolution may take a while)...'
    & $mamba $action --yes --prefix $prefix --override-channels -c conda-forge 'ffmpeg=9.0.2'
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg 9 provisioning failed.' }
    if (!(Test-Path (Join-Path $InstallDir 'ffmpeg9\Library\bin\avcodec-63.dll'))) { throw 'FFmpeg 9 DLLs were not installed.' }
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
  Write-Host "Installed Remedia $($release.tag_name) to $InstallDir"
  Write-Host "Add $binDir to PATH, then run: remedia --version"
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
