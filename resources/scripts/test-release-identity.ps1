[CmdletBinding()]
param(
  [ValidatePattern('^\d+\.\d+\.\d+$')]
  [string]$ExpectedVersion,

  [string]$ReleaseDirectory
)

$ErrorActionPreference = 'Stop'

function Assert-LastExitCode {
  param([Parameter(Mandatory = $true)][string]$Action)

  if ($LASTEXITCODE -ne 0) {
    throw "$Action failed with exit code $LASTEXITCODE."
  }
}

function Read-CommandLineVersion {
  param([Parameter(Mandatory = $true)][string]$Path)

  $startInfo = [Diagnostics.ProcessStartInfo]::new()
  $startInfo.FileName = $Path
  $startInfo.ArgumentList.Add('--version')
  $startInfo.UseShellExecute = $false
  $startInfo.CreateNoWindow = $true
  $startInfo.RedirectStandardOutput = $true
  $startInfo.RedirectStandardError = $true

  $process = [Diagnostics.Process]::new()
  $process.StartInfo = $startInfo
  if (!$process.Start()) {
    throw "Failed to start version query for $Path."
  }

  try {
    $standardOutput = $process.StandardOutput.ReadToEnd()
    $standardError = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
      throw "Version query for $Path failed with exit code $($process.ExitCode)."
    }
    return "$standardOutput`n$standardError".Trim()
  }
  finally {
    $process.Dispose()
  }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repoRoot

try {
  $metadataJson = (& cargo metadata --locked --no-deps --format-version 1) -join "`n"
  Assert-LastExitCode 'Reading Cargo workspace metadata'
  $metadata = $metadataJson | ConvertFrom-Json
  $workspacePackages = @(
    $metadata.packages | Where-Object {
      $metadata.workspace_members -contains $_.id
    }
  )
  $workspaceVersions = @(
    $workspacePackages.version | Sort-Object -Unique
  )

  if ($workspacePackages.Count -eq 0 -or $workspaceVersions.Count -ne 1) {
    $versionSummary = ($workspaceVersions -join ', ')
    throw "Workspace packages do not share one release version: $versionSummary"
  }

  $sourceVersion = [string]$workspaceVersions[0]
  if ($ExpectedVersion -and $ExpectedVersion -ne $sourceVersion) {
    throw "Requested version $ExpectedVersion does not match authoritative workspace version $sourceVersion."
  }

  $toolchainText = Get-Content -LiteralPath (Join-Path $repoRoot 'rust-toolchain.toml') -Raw
  $channelMatch = [regex]::Match(
    $toolchainText,
    '(?m)^channel\s*=\s*"nightly-(\d{4}-\d{2}-\d{2})"\s*$'
  )
  if (!$channelMatch.Success) {
    throw 'rust-toolchain.toml must pin a dated nightly toolchain.'
  }

  $pinnedDate = $channelMatch.Groups[1].Value
  $compilerDetails = (& rustc -Vv) -join "`n"
  Assert-LastExitCode 'Reading Rust compiler identity'
  $compilerDateMatch = [regex]::Match(
    $compilerDetails,
    '(?m)^commit-date:\s*(\d{4}-\d{2}-\d{2})\s*$'
  )
  if (!$compilerDateMatch.Success) {
    throw 'rustc -Vv did not report a compiler commit date.'
  }
  if ($compilerDateMatch.Groups[1].Value -ne $pinnedDate) {
    throw "Active compiler date $($compilerDateMatch.Groups[1].Value) does not match pinned nightly date $pinnedDate."
  }

  if ($ReleaseDirectory) {
    $resolvedReleaseDirectory = (Resolve-Path -LiteralPath $ReleaseDirectory).Path
    $binaries = @(
      @{ Name = 'WM'; Path = Join-Path $resolvedReleaseDirectory 'glazewm.exe'; HasCliVersion = $true },
      @{ Name = 'Watcher'; Path = Join-Path $resolvedReleaseDirectory 'glazewm-watcher.exe'; HasCliVersion = $false },
      @{ Name = 'CLI'; Path = Join-Path $resolvedReleaseDirectory 'glazewm-cli.exe'; HasCliVersion = $true }
    )
    $expectedWindowsVersion = "$sourceVersion.0"

    foreach ($binary in $binaries) {
      if (!(Test-Path -LiteralPath $binary.Path -PathType Leaf)) {
        throw "$($binary.Name) release binary is missing: $($binary.Path)"
      }

      $versionInfo = (Get-Item -LiteralPath $binary.Path).VersionInfo
      if ($versionInfo.FileVersion -ne $expectedWindowsVersion) {
        throw "$($binary.Name) FileVersion $($versionInfo.FileVersion) does not match $expectedWindowsVersion."
      }
      if ($versionInfo.ProductVersion -ne $expectedWindowsVersion) {
        throw "$($binary.Name) ProductVersion $($versionInfo.ProductVersion) does not match $expectedWindowsVersion."
      }

      if ($binary.HasCliVersion) {
        $reportedVersion = Read-CommandLineVersion -Path $binary.Path
        if ($reportedVersion -notmatch "(?m)(^|\s)$([regex]::Escape($sourceVersion))(\s|$)") {
          throw "$($binary.Name) command-line version does not report $sourceVersion."
        }
      }
    }
  }

  Write-Host "Release identity validated: version $sourceVersion, nightly $pinnedDate."
}
finally {
  Pop-Location
}
