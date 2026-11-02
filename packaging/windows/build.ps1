<#
.SYNOPSIS
  Build a distributable Tether host package: dist\tether-host-windows-<version>.zip

.DESCRIPTION
  Release-builds tetherd.exe and tetherd-agent.exe, optionally Authenticode-signs
  them, and zips them with install.ps1 / uninstall.ps1.

  Signing: set TETHER_SIGN_PFX (path to a code-signing .pfx) and
  TETHER_SIGN_PASSWORD, with signtool.exe on PATH (Windows SDK). Unsigned
  builds work but SmartScreen and Defender warn loudly about an unsigned
  remote-control binary; an OV certificate builds reputation over time, an EV
  certificate has it immediately.

  Prerequisites: Rust (MSVC toolchain), CMake and NASM (turbojpeg) - see README.
#>
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location $root
try {
  cargo build --release -p tetherd
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

  $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
  $stage = Join-Path $root "dist\tether-host-windows-$version"
  Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path $stage | Out-Null
  Copy-Item target\release\tetherd.exe, target\release\tetherd-agent.exe $stage
  Copy-Item packaging\windows\install.ps1, packaging\windows\uninstall.ps1 $stage

  if ($env:TETHER_SIGN_PFX) {
    foreach ($exe in Get-ChildItem $stage -Filter *.exe) {
      signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
        /f $env:TETHER_SIGN_PFX /p $env:TETHER_SIGN_PASSWORD $exe.FullName
      if ($LASTEXITCODE -ne 0) { throw "signing $($exe.Name) failed" }
    }
  } else {
    Write-Warning "TETHER_SIGN_PFX not set: binaries are unsigned (SmartScreen will warn)."
  }

  @"
Tether host for Windows $version

Install (PowerShell, from this folder):
  powershell -ExecutionPolicy Bypass -File .\install.ps1 -Signal wss://your-server/ws

Then pair a device:
  & "`$env:LOCALAPPDATA\Programs\Tether\tetherd.exe" pair

Uninstall:
  powershell -ExecutionPolicy Bypass -File "`$env:LOCALAPPDATA\Programs\Tether\uninstall.ps1"
"@ | Set-Content -Encoding utf8 (Join-Path $stage "README.txt")

  $zip = "$stage.zip"
  Remove-Item $zip -ErrorAction SilentlyContinue
  Compress-Archive -Path "$stage\*" -DestinationPath $zip
  Write-Host "Built $zip"
} finally {
  Pop-Location
}
