<#
.SYNOPSIS
  Installs the Virtual Display Driver for SecondScrn (run elevated).

.DESCRIPTION
  Obtains the official, SignPath-signed VirtualDrivers package through
  winget (machine scope), then installs the driver with install-vdd.ps1.
  SecondScrn does not redistribute or modify the driver.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$id = 'VirtualDrivers.Virtual-Display-Driver'

function Find-Package {
    $roots = @("$env:ProgramFiles\WinGet\Packages", "$env:LOCALAPPDATA\Microsoft\WinGet\Packages")
    foreach ($r in $roots) {
        $d = Get-ChildItem $r -Directory -Filter "$id*" -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($d) { return $d.FullName }
    }
    return $null
}

$pkg = Find-Package
if (-not $pkg) {
    Write-Host "Downloading the Virtual Display Driver package via winget..."
    winget install --id $id -e --scope machine --silent --accept-source-agreements --accept-package-agreements
    $pkg = Find-Package
}
if (-not $pkg) { throw "The Virtual Display Driver package could not be obtained via winget." }

$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'ARM64' } else { 'x86' }  # 'x86' holds the x64 build upstream
$driverDir = Join-Path $pkg "SignedDrivers\$arch\VDD"
$devcon = Join-Path $pkg 'Dependencies\devcon.exe'

& (Join-Path $here 'install-vdd.ps1') -DriverDir $driverDir -DevconPath $devcon
exit $LASTEXITCODE
