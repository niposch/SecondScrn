<#
.SYNOPSIS
  Removes the Virtual Display Driver device and driver package (run elevated).

.DESCRIPTION
  Not run automatically by the SecondScrn uninstaller: other software may use
  the same driver. Run it manually only if nothing else needs VDD.
#>
[CmdletBinding()]
param([switch] $KeepSettings)

$ErrorActionPreference = 'Stop'
$devices = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains 'Root\MttVDD' }
foreach ($d in $devices) {
    Write-Host "Removing device $($d.InstanceId)"
    pnputil /remove-device "$($d.InstanceId)" | Out-Null
}
$drivers = pnputil /enum-drivers | Out-String
foreach ($m in [regex]::Matches($drivers, '(oem\d+\.inf)\s*\r?\n\s*Original Name:\s*mttvdd\.inf', 'IgnoreCase')) {
    Write-Host "Deleting driver package $($m.Groups[1].Value)"
    pnputil /delete-driver $m.Groups[1].Value /uninstall /force | Out-Null
}
if (-not $KeepSettings -and (Test-Path 'C:\VirtualDisplayDriver')) {
    Remove-Item 'C:\VirtualDisplayDriver' -Recurse -Force
}
Write-Host 'Virtual Display Driver removed.'
