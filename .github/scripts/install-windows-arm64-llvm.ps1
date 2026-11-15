$ErrorActionPreference = 'Stop'

# conda-forge does not ship Clang 19.1.7 for win-arm64. Bindgen runs inside the
# native ARM64 rustc process and must load an ARM64 DLL, even when x64 tools run
# successfully under emulation. This installs build tooling only.
$installer = Join-Path $env:RUNNER_TEMP 'LLVM-19.1.7-woa64.exe'
$prefix = Join-Path $env:RUNNER_TEMP 'remedia-llvm-arm64'
$url = 'https://github.com/llvm/llvm-project/releases/download/llvmorg-19.1.7/LLVM-19.1.7-woa64.exe'
$expected = '08d871c9a20caf0987ede1a4e44d489e28854c9f2fff3da98b486891f4856e05'
Invoke-WebRequest -Uri $url -OutFile $installer
if ((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash -ine $expected) {
  throw 'LLVM ARM64 installer SHA-256 mismatch.'
}

# NSIS requires /D to be the final argument, including paths containing spaces.
$process = Start-Process -FilePath $installer -ArgumentList @('/S', "/D=$prefix") -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "LLVM ARM64 installation failed: $($process.ExitCode)" }
$bin = Join-Path $prefix 'bin'
$dll = Join-Path $bin 'libclang.dll'
if (!(Test-Path -LiteralPath $dll)) { throw "Native libclang is missing: $dll" }
$stream = [IO.File]::OpenRead($dll)
$reader = [IO.BinaryReader]::new($stream)
try {
  $stream.Position = 0x3c
  $header = $reader.ReadInt32()
  $stream.Position = $header
  if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0xaa64) {
    throw 'LLVM libclang.dll must be a native ARM64 PE image.'
  }
} finally {
  $reader.Dispose()
}
"REMEDIA_BUILD_LIBCLANG=$bin" >> $env:GITHUB_ENV
