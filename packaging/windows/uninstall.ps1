<#
.SYNOPSIS
  Remove the Tether host for the current user.

.DESCRIPTION
  Stops and unregisters the "Tether Host" logon task, removes the firewall rule
  (if one was added and this shell is elevated) and the installed binaries.

  Your pairing state - %APPDATA%\tether (host key, paired devices, config) -
  is kept unless you confirm deleting it or pass -RemoveState: deleting it
  means every phone has to pair again.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param([switch]$RemoveState)
$ErrorActionPreference = "Stop"

$TaskName = "Tether Host"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\Tether"
$StateDir = Join-Path $env:APPDATA "tether"

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
  if ($PSCmdlet.ShouldProcess($TaskName, "Stop and unregister")) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
  }
}
Get-Process tetherd, tetherd-agent -ErrorAction SilentlyContinue |
  Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
  Stop-Process -Force -ErrorAction SilentlyContinue

if (Get-NetFirewallRule -DisplayName "Tether LAN" -ErrorAction SilentlyContinue) {
  try {
    Remove-NetFirewallRule -DisplayName "Tether LAN"
  } catch {
    Write-Warning "Could not remove the 'Tether LAN' firewall rule (needs an elevated shell)."
  }
}

if ((Test-Path $InstallDir) -and $PSCmdlet.ShouldProcess($InstallDir, "Remove binaries")) {
  Start-Sleep -Milliseconds 500 # let the processes release their files
  Remove-Item -Recurse -Force $InstallDir
}

if (Test-Path $StateDir) {
  $remove = $RemoveState
  if (-not $remove) {
    $answer = Read-Host "Also delete pairing state in $StateDir? Every device would need to pair again. [y/N]"
    $remove = $answer -match '^(y|yes)$'
  }
  if ($remove -and $PSCmdlet.ShouldProcess($StateDir, "Delete pairing state")) {
    Remove-Item -Recurse -Force $StateDir
    Write-Host "Removed $StateDir."
  } else {
    Write-Host "Kept $StateDir (host key, paired devices, config)."
  }
}
Write-Host "Tether host uninstalled."
