<#
.SYNOPSIS
  Builds the Windows release: Rust core, WinUI app (self-contained) and the
  Velopack installer/update packages.

.EXAMPLE
  ./packaging/windows/build-release.ps1 -Version 0.0.1
  Output: releases/ (Setup.exe, full/delta .nupkg, RELEASES feed files)

.NOTES
  Code signing: pass -SignParams (forwarded to vpk --signParams, e.g.
  "/a /fd sha256 /tr http://timestamp.digicert.com /td sha256") once a
  certificate is available. Unsigned builds work but trigger SmartScreen.
#>
param(
    [Parameter(Mandatory)] [string] $Version,
    [string] $Channel = 'win',
    [string] $SignParams,
    [string] $OutputDir = 'releases'
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Push-Location $root
try {
    Write-Host "== Rust core (release)"
    cargo build -p secondscrn-windows-host --release
    if ($LASTEXITCODE) { throw 'cargo build failed' }

    Write-Host "== WinUI app"
    $publish = Join-Path $root 'target\publish\win-x64'
    $publish = [IO.Path]::GetFullPath($publish)
    $allowedPublishRoot = [IO.Path]::GetFullPath((Join-Path $root 'target\publish')) + [IO.Path]::DirectorySeparatorChar
    if (-not $publish.StartsWith($allowedPublishRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Publish path is outside the workspace target/publish directory'
    }
    if (Test-Path -LiteralPath $publish) { Remove-Item -LiteralPath $publish -Recurse -Force }
    dotnet publish apps/windows/SecondScrn.App/SecondScrn.App.csproj -c Release -r win-x64 --self-contained `
        -p:Version=$Version -p:RustProfile=release -o $publish
    if ($LASTEXITCODE) { throw 'dotnet publish failed' }
    foreach ($required in 'secondscrn_host.dll', 'secondscrn-host-cli.exe', 'SecondScrn.pri', 'SecondScrn.exe', 'Driver\setup-vdd.ps1', 'THIRD_PARTY_NOTICES.md', 'Licenses\RUST-NOTICES.txt') {
        if (-not (Test-Path (Join-Path $publish $required))) { throw "$required missing from publish output" }
    }

    Write-Host "== Velopack"
    $args = @('pack', '--packId', 'SecondScrn', '--packVersion', $Version, '--packDir', $publish,
              '--mainExe', 'SecondScrn.exe', '--packTitle', 'SecondScrn', '--packAuthors', 'SecondScrn contributors',
              '--icon', 'apps/windows/SecondScrn.App/Assets/SecondScrn.ico', '--channel', $Channel,
              '--outputDir', $OutputDir)
    if ($SignParams) { $args += @('--signParams', $SignParams) }
    vpk @args
    if ($LASTEXITCODE) { throw 'vpk pack failed' }
    Get-ChildItem $OutputDir | Format-Table Name, Length
}
finally {
    Pop-Location
}
