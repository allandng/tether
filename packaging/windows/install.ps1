<#
.SYNOPSIS
  Install the Tether host for the current Windows user and start it at every sign-in.

.DESCRIPTION
  Per-user install (no administrator rights needed, except for -LanPort):
    1. copies tetherd.exe + tetherd-agent.exe to %LOCALAPPDATA%\Programs\Tether
    2. writes %APPDATA%\tether\config.toml with the signal server and secret
       (passed to `tetherd setup` on stdin - never on a command line)
    3. registers the "Tether Host" Scheduled Task: runs tetherd-agent.exe at
       sign-in, as you, in your desktop session (a Windows service would run
       in session 0 and could not see or control your desktop)
    4. starts it now.

  Then arm a pairing code with:  & "$env:LOCALAPPDATA\Programs\Tether\tetherd.exe" pair

.PARAMETER Signal
  Signal server: host:port, or a full wss:// URL (e.g. wss://tether.example.com/ws).
.PARAMETER Secret
  The signal server's shared secret. Prompted for (hidden) when omitted -
  preferred: passing -Secret puts it in this shell's own command line and
  history.
.PARAMETER Codec
  h264 (default; ~10x less bandwidth) or jpeg.
.PARAMETER Audio
  Also stream this PC's system audio to controllers.
.PARAMETER DeviceId
  Name to register under (default: the computer name).
.PARAMETER LanPort
  Also accept direct LAN connections on this port: adds an inbound firewall
  rule (needs an elevated PowerShell). The WebRTC path needs no inbound rule.
.PARAMETER Source
  Folder holding tetherd.exe and tetherd-agent.exe (default: next to this script,
  falling back to the repo's target\release).
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][string]$Signal,
  [string]$Secret,
  [ValidateSet("h264", "jpeg")][string]$Codec = "h264",
  [switch]$Audio,
  [string]$DeviceId,
  [int]$LanPort = 0,
  [string]$Source
)
$ErrorActionPreference = "Stop"

$TaskName = "Tether Host"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\Tether"

if (-not $Source) {
  $Source = $PSScriptRoot
  if (-not (Test-Path (Join-Path $Source "tetherd.exe"))) {
    $Source = Join-Path $PSScriptRoot "..\..\target\release"
  }
}
foreach ($exe in "tetherd.exe", "tetherd-agent.exe") {
  if (-not (Test-Path (Join-Path $Source $exe))) {
    throw "$exe not found in $Source (build with: cargo build --release -p tetherd)"
  }
}

if (-not $Secret) {
  $secure = Read-Host "Signal secret" -AsSecureString
  $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
  try { $Secret = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
  finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
}
if (-not $Secret) { throw "a secret is required" }

# Stop a running copy first so its binaries can be replaced.
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
  if ($PSCmdlet.ShouldProcess($TaskName, "Stop existing task")) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
  }
}
Get-Process tetherd, tetherd-agent -ErrorAction SilentlyContinue |
  Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
  Stop-Process -Force -ErrorAction SilentlyContinue

if ($PSCmdlet.ShouldProcess($InstallDir, "Install tetherd")) {
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Copy-Item (Join-Path $Source "tetherd.exe") $InstallDir -Force
  Copy-Item (Join-Path $Source "tetherd-agent.exe") $InstallDir -Force
  Copy-Item (Join-Path $PSScriptRoot "uninstall.ps1") $InstallDir -Force -ErrorAction SilentlyContinue
}
$Tetherd = Join-Path $InstallDir "tetherd.exe"
$Agent = Join-Path $InstallDir "tetherd-agent.exe"

# Config: the secret goes in on stdin, so it never shows in a process list.
$setupArgs = @("setup", "--signal", $Signal, "--codec", $Codec)
if ($DeviceId) { $setupArgs += @("--device-id", $DeviceId) }
if ($Audio) { $setupArgs += "--audio" }
if ($PSCmdlet.ShouldProcess("$env:APPDATA\tether\config.toml", "Write config")) {
  # Windows PowerShell pipes to native stdin in $OutputEncoding (ASCII by
  # default), which would mangle a non-ASCII secret: send UTF-8, no BOM.
  $prevEncoding = $OutputEncoding
  $OutputEncoding = [System.Text.UTF8Encoding]::new($false)
  try { $Secret | & $Tetherd @setupArgs }
  finally { $OutputEncoding = $prevEncoding }
  if ($LASTEXITCODE -ne 0) { throw "tetherd setup failed" }
}
if ($LanPort -gt 0) {
  Write-Warning ('LAN transport: add bind = "<this PC''s LAN IP>", port = ' + $LanPort +
    ' and allow = ["<controller IP>"] to ' + (Join-Path $env:APPDATA 'tether\config.toml'))
}

# Logon task: interactive, as this user, no time limit, restart on failure.
$action = New-ScheduledTaskAction -Execute $Agent -WorkingDirectory $InstallDir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
  -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
  -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew
if ($PSCmdlet.ShouldProcess($TaskName, "Register logon task")) {
  Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings `
    -Description "Tether remote-desktop host: streams this desktop to your paired devices." `
    -Force | Out-Null
}

if ($LanPort -gt 0) {
  $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
  if (-not $admin) {
    Write-Warning "Skipping the LAN firewall rule: rerun from an elevated PowerShell to add it."
  } elseif ($PSCmdlet.ShouldProcess("Windows Firewall", "Allow TCP $LanPort for tetherd")) {
    Remove-NetFirewallRule -DisplayName "Tether LAN" -ErrorAction SilentlyContinue
    New-NetFirewallRule -DisplayName "Tether LAN" -Direction Inbound -Action Allow `
      -Protocol TCP -LocalPort $LanPort -Program $Tetherd -Profile Private | Out-Null
  }
}

if ($PSCmdlet.ShouldProcess($TaskName, "Start now")) {
  Start-ScheduledTask -TaskName $TaskName
  Start-Sleep -Seconds 2
  & $Tetherd status
}

Write-Host ""
Write-Host "Installed. Tether starts automatically when you sign in."
Write-Host "Pair a phone or browser:  & `"$Tetherd`" pair"
Write-Host "Logs:                     $env:LOCALAPPDATA\Tether\tetherd.log"
Write-Host "Keep this PC reachable: disable sleep (Settings > System > Power) - a sleeping PC can't be reached."
