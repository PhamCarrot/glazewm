param(
  [Parameter(Mandatory = $true)]
  [ValidatePattern('^\d+\.\d+\.\d+$')]
  [string]$VersionNumber
)

$ErrorActionPreference = 'Stop'

function Assert-LastExitCode {
  param([Parameter(Mandatory = $true)][string]$Action)

  if ($LASTEXITCODE -ne 0) {
    throw "$Action failed with exit code $LASTEXITCODE."
  }
}

function Assert-WixExtension {
  param([Parameter(Mandatory = $true)][string]$Extension)

  $installedExtensions = (& wix extension list 2>&1) -join "`n"
  Assert-LastExitCode 'Listing WiX extensions'

  if ($installedExtensions -notmatch [regex]::Escape($Extension)) {
    throw "WiX extension $Extension/5 is required. Install it explicitly before packaging."
  }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repoRoot

try {
  $releaseName = "glazewm-combined-v$VersionNumber-windows-x64"
  $outDir = Join-Path $repoRoot 'out'
  $stageDir = Join-Path $outDir $releaseName
  $stageCliDir = Join-Path $stageDir 'cli'
  $zipPath = Join-Path $outDir "$releaseName.zip"
  $msiPath = Join-Path $outDir "$releaseName.msi"

  if (Test-Path -LiteralPath $stageDir) {
    Remove-Item -LiteralPath $stageDir -Recurse -Force
  }

  New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
  New-Item -ItemType Directory -Path $stageCliDir -Force | Out-Null
  New-Item -ItemType Directory -Path $outDir -Force | Out-Null

  $env:VERSION_NUMBER = $VersionNumber

  Write-Host 'Running focused release checks...'
  & cargo test -p wm-common
  Assert-LastExitCode 'wm-common tests'
  & cargo test -p wm
  Assert-LastExitCode 'wm tests'
  & cargo check -p wm-platform
  Assert-LastExitCode 'wm-platform check'

  Write-Host 'Building optimized Windows binaries...'
  & cargo build --locked --release --workspace
  Assert-LastExitCode 'Cargo release build'

  $rootBinaries = @(
    'target\release\glazewm.exe',
    'target\release\glazewm-watcher.exe'
  )

  foreach ($relativePath in $rootBinaries) {
    $sourcePath = Join-Path $repoRoot $relativePath
    if (!(Test-Path -LiteralPath $sourcePath)) {
      throw "Required build output is missing: $sourcePath"
    }
    Copy-Item -LiteralPath $sourcePath -Destination $stageDir -Force
  }

  $cliSourcePath = Join-Path $repoRoot 'target\release\glazewm-cli.exe'
  if (!(Test-Path -LiteralPath $cliSourcePath)) {
    throw "Required build output is missing: $cliSourcePath"
  }
  Copy-Item -LiteralPath $cliSourcePath `
    -Destination (Join-Path $stageCliDir 'glazewm.exe') -Force

  Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE.md') -Destination $stageDir -Force
  Copy-Item -LiteralPath (Join-Path $repoRoot 'README.md') -Destination $stageDir -Force
  Copy-Item -LiteralPath (Join-Path $repoRoot 'resources\assets\sample-config.yaml') `
    -Destination (Join-Path $stageDir 'config.yaml') -Force

  if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
  }

  Write-Host 'Creating portable ZIP...'
  Compress-Archive -Path (Join-Path $stageDir '*') -DestinationPath $zipPath -CompressionLevel Optimal

  if (!(Get-Command wix -ErrorAction SilentlyContinue)) {
    throw 'WiX Toolset 5 is required to build the MSI. Install it explicitly or use the portable ZIP created above.'
  }

  Assert-WixExtension 'WixToolset.UI.wixext'
  Assert-WixExtension 'WixToolset.Util.wixext'

  Write-Host 'Creating unsigned x64 MSI...'
  & wix build `
    -arch x64 `
    -ext WixToolset.UI.wixext `
    -ext WixToolset.Util.wixext `
    -out $msiPath `
    '.\resources\wix\combined-standalone.wxs' `
    '.\resources\wix\standalone-ui.wxs' `
    -d VERSION_NUMBER="$VersionNumber" `
    -d EXE_DIR="$stageDir"
  Assert-LastExitCode 'Building MSI'

  Write-Host ''
  Write-Host 'Release artifacts created:'
  Write-Host "  Portable ZIP: $zipPath"
  Write-Host "  Installer MSI: $msiPath"
  Write-Host ''
  Write-Warning 'These personal artifacts are unsigned. Windows may show an Unknown publisher warning.'
}
finally {
  Pop-Location
}
