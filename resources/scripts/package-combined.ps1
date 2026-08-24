param(
  [Parameter(Mandatory = $true)]
  [ValidatePattern('^\d+\.\d+\.\d+$')]
  [string]$VersionNumber,

  [switch]$UiAccess,

  [string]$SigningCertificateThumbprint,

  [string]$TimestampUrl = 'http://timestamp.digicert.com'
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

function Resolve-SignTool {
  $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($command) {
    return $command.Source
  }

  $kitsRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
  $candidate = Get-ChildItem -LiteralPath $kitsRoot -Filter signtool.exe `
    -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1

  if (!$candidate) {
    throw 'Windows SDK signtool.exe is required for a UIAccess package.'
  }

  return $candidate.FullName
}

function Sign-PackageFiles {
  param(
    [Parameter(Mandatory = $true)][string[]]$Paths,
    [Parameter(Mandatory = $true)][string]$SignTool
  )

  foreach ($path in $Paths) {
    & $SignTool sign /sha1 $SigningCertificateThumbprint /fd SHA256 `
      /tr $TimestampUrl /td SHA256 /v $path
    Assert-LastExitCode "Signing $path"

    $signature = Get-AuthenticodeSignature -LiteralPath $path
    if (!$signature.SignerCertificate) {
      throw "No Authenticode signature was embedded in $path."
    }
    if ($signature.SignerCertificate.Thumbprint -ne $SigningCertificateThumbprint) {
      throw "Unexpected signing certificate on $path."
    }
  }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repoRoot

try {
  $releaseSuffix = if ($UiAccess) { '-uiaccess' } else { '' }
  $releaseName = "glazewm-combined-v$VersionNumber-windows-x64$releaseSuffix"
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

  if ($UiAccess -and !$SigningCertificateThumbprint) {
    throw 'A UIAccess package requires -SigningCertificateThumbprint.'
  }

  $signTool = if ($UiAccess) { Resolve-SignTool } else { $null }

  Write-Host 'Building optimized Windows binaries...'
  $buildArgs = @('build', '--locked', '--release', '--workspace')
  if ($UiAccess) {
    $buildArgs += @('--features', 'wm/ui_access')
  }
  & cargo @buildArgs
  Assert-LastExitCode 'Cargo release build'

  $rootBinaries = @(
    'target\release\glazewm.exe',
    'target\release\glazewm-watcher.exe'
  )

  if ($UiAccess) {
    $binariesToSign = @(
      'target\release\glazewm.exe',
      'target\release\glazewm-watcher.exe',
      'target\release\glazewm-cli.exe'
    ) | ForEach-Object { Join-Path $repoRoot $_ }

    Sign-PackageFiles -Paths $binariesToSign -SignTool $signTool

    $manifestBytes = [IO.File]::ReadAllBytes($binariesToSign[0])
    $manifestText = [Text.Encoding]::ASCII.GetString($manifestBytes)
    if ($manifestText -notmatch 'uiAccess="true"') {
      throw 'glazewm.exe was not built with uiAccess="true".'
    }
  }

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

  if ($UiAccess) {
    Write-Host 'Creating UIAccess staging ZIP...'
  }
  else {
    Write-Host 'Creating portable ZIP...'
  }
  Compress-Archive -Path (Join-Path $stageDir '*') -DestinationPath $zipPath -CompressionLevel Optimal

  if (!(Get-Command wix -ErrorAction SilentlyContinue)) {
    throw 'WiX Toolset 5 is required to build the MSI. Install it explicitly or use the portable ZIP created above.'
  }

  Assert-WixExtension 'WixToolset.UI.wixext'
  Assert-WixExtension 'WixToolset.Util.wixext'

  Write-Host 'Creating x64 MSI...'
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

  if ($UiAccess) {
    Sign-PackageFiles -Paths @($msiPath) -SignTool $signTool
  }

  Write-Host ''
  Write-Host 'Release artifacts created:'
  if ($UiAccess) {
    Write-Host "  UIAccess staging ZIP: $zipPath"
  }
  else {
    Write-Host "  Portable ZIP: $zipPath"
  }
  Write-Host "  Installer MSI: $msiPath"
  Write-Host ''
  if ($UiAccess) {
    Write-Warning 'This UIAccess package only runs after its signing certificate is trusted and it is installed in a secure location.'
  }
  else {
    Write-Warning 'These personal artifacts are unsigned. Windows may show an Unknown publisher warning.'
  }
}
finally {
  Pop-Location
}
