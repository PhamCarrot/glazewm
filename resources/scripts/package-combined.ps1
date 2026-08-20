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

function Ensure-WixExtension {
  param([Parameter(Mandatory = $true)][string]$Extension)

  $installedExtensions = (& wix extension list 2>&1) -join "`n"
  if ($installedExtensions -notmatch [regex]::Escape($Extension)) {
    & wix extension add "$Extension/5"
    Assert-LastExitCode "Installing WiX extension $Extension"
  }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repoRoot

try {
  $releaseName = "glazewm-combined-v$VersionNumber-windows-x64"
  $outDir = Join-Path $repoRoot 'out'
  $stageDir = Join-Path $outDir $releaseName
  $zipPath = Join-Path $outDir "$releaseName.zip"
  $msiPath = Join-Path $outDir "$releaseName.msi"

  if (Test-Path -LiteralPath $stageDir) {
    Remove-Item -LiteralPath $stageDir -Recurse -Force
  }

  New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
  New-Item -ItemType Directory -Path $outDir -Force | Out-Null

  $env:VERSION_NUMBER = $VersionNumber

  Write-Host 'Running test suite...'
  & cargo test --workspace --lib --bins --tests
  Assert-LastExitCode 'Cargo tests'

  Write-Host 'Building optimized Windows binaries...'
  & cargo build --locked --release --workspace
  Assert-LastExitCode 'Cargo release build'

  $requiredFiles = @(
    'target\release\glazewm.exe',
    'target\release\glazewm-cli.exe',
    'target\release\glazewm-watcher.exe'
  )

  foreach ($relativePath in $requiredFiles) {
    $sourcePath = Join-Path $repoRoot $relativePath
    if (!(Test-Path -LiteralPath $sourcePath)) {
      throw "Required build output is missing: $sourcePath"
    }
    Copy-Item -LiteralPath $sourcePath -Destination $stageDir -Force
  }

  Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE.md') -Destination $stageDir -Force
  Copy-Item -LiteralPath (Join-Path $repoRoot 'README.md') -Destination $stageDir -Force
  Copy-Item -LiteralPath (Join-Path $repoRoot 'resources\assets\sample-config.yaml') `
    -Destination (Join-Path $stageDir 'config.example.yaml') -Force

  if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
  }

  Write-Host 'Creating portable ZIP...'
  Compress-Archive -Path (Join-Path $stageDir '*') -DestinationPath $zipPath -CompressionLevel Optimal

  if (!(Get-Command dotnet -ErrorAction SilentlyContinue)) {
    throw '.NET SDK is required to install WiX. Install it from https://dotnet.microsoft.com/download'
  }

  $installedSdks = (& dotnet --list-sdks 2>$null) -join "`n"
  if ([string]::IsNullOrWhiteSpace($installedSdks)) {
    throw '.NET is installed, but no .NET SDK was found. Install a current .NET SDK before building the MSI.'
  }

  if (!(Get-Command wix -ErrorAction SilentlyContinue)) {
    Write-Host 'Installing WiX Toolset 5...'
    & dotnet tool install --global wix --version 5.0.0
    Assert-LastExitCode 'Installing WiX Toolset'
    $env:PATH = "$env:PATH;$env:USERPROFILE\.dotnet\tools"
  }

  Ensure-WixExtension 'WixToolset.UI.wixext'
  Ensure-WixExtension 'WixToolset.Util.wixext'

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
