[CmdletBinding()]
param(
  [ValidateRange(512, 12288)]
  [int]$MemoryMegabytes = 4096,

  [ValidateRange(1024, 8192)]
  [int]$MinimumAvailableMegabytes = 2048,

  [ValidateRange(2, 20)]
  [int]$WindowCount = 10,

  [ValidateRange(1, 20)]
  [int]$Cycles = 5,

  [ValidateRange(10, 1000)]
  [int]$DelayMs = 20,

  [string]$CliPath
)

$ErrorActionPreference = 'Stop'
$HelperSource = Join-Path $PSScriptRoot 'memory-pressure-helper.cs'
$StressScript = Join-Path $PSScriptRoot 'animation-stress.ps1'
$TargetDirectory = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\target'))
$RunId = Get-Date -Format 'yyyyMMdd-HHmmss'
$ReportPath = Join-Path $TargetDirectory "animation-memory-pressure-report-$RunId.json"
$Lease = $null
$StressReport = $null
$Failure = $null

function Get-MemorySnapshot {
  $os = Get-CimInstance Win32_OperatingSystem
  $chrome = @(Get-Process chrome -ErrorAction SilentlyContinue)
  $spotify = @(Get-Process Spotify -ErrorAction SilentlyContinue)
  return [ordered]@{
    total_ram_mb = [Math]::Round($os.TotalVisibleMemorySize / 1KB, 3)
    available_ram_mb = [Math]::Round($os.FreePhysicalMemory / 1KB, 3)
    committed_memory_mb = [Math]::Round(
      ($os.TotalVirtualMemorySize - $os.FreeVirtualMemory) / 1KB,
      3)
    chrome_processes = $chrome.Count
    chrome_working_set_mb = [Math]::Round(
      (($chrome | Measure-Object WorkingSet64 -Sum).Sum) / 1MB,
      3)
    spotify_processes = $spotify.Count
    spotify_working_set_mb = [Math]::Round(
      (($spotify | Measure-Object WorkingSet64 -Sum).Sum) / 1MB,
      3)
  }
}

if (-not (Test-Path -LiteralPath $HelperSource -PathType Leaf)) {
  throw "Memory-pressure helper source not found: $HelperSource"
}
if (-not (Test-Path -LiteralPath $StressScript -PathType Leaf)) {
  throw "Animation stress script not found: $StressScript"
}

Add-Type -Path $HelperSource
$Before = Get-MemorySnapshot
if (($Before.available_ram_mb - $MemoryMegabytes) -lt $MinimumAvailableMegabytes) {
  throw "Refusing to allocate $MemoryMegabytes MB: only $($Before.available_ram_mb) MB is available, and the safety floor is $MinimumAvailableMegabytes MB."
}

$StartedAt = Get-Date
try {
  $Lease = [GlazeWmDiagnostics.MemoryPressureLease]::new($MemoryMegabytes)
  $UnderPressure = Get-MemorySnapshot
  if ($UnderPressure.available_ram_mb -lt $MinimumAvailableMegabytes) {
    throw "Available RAM fell below the $MinimumAvailableMegabytes MB safety floor after allocation."
  }

  $powershell = "$env:WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe"
  $stressArguments = @(
    '-NoProfile',
    '-ExecutionPolicy', 'Bypass',
    '-File', $StressScript,
    '-WindowCount', $WindowCount,
    '-Cycles', $Cycles,
    '-DelayMs', $DelayMs
  )
  if ($CliPath) {
    $stressArguments += @('-CliPath', [IO.Path]::GetFullPath($CliPath))
  }
  $startParameters = @{
    FilePath = $powershell
    ArgumentList = $stressArguments
    WindowStyle = 'Hidden'
    PassThru = $true
    Wait = $true
  }
  $stressProcess = Start-Process @startParameters
  if ($stressProcess.ExitCode -ne 0) {
    throw "Animation stress child exited with code $($stressProcess.ExitCode)."
  }

  $stressReportFile = Get-ChildItem -LiteralPath $TargetDirectory -Filter 'animation-stress-report-*.json' |
    Where-Object LastWriteTime -ge $StartedAt |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
  if ($null -eq $stressReportFile) {
    throw 'The animation stress child did not produce a report.'
  }
  $StressReport = Get-Content -Raw -LiteralPath $stressReportFile.FullName | ConvertFrom-Json
} catch {
  $Failure = $_.Exception.Message
} finally {
  if ($null -ne $Lease) {
    $Lease.Dispose()
  }
  Start-Sleep -Seconds 2
  $AfterRelease = Get-MemorySnapshot
}

$Report = [pscustomobject][ordered]@{
  run_id = $RunId
  passed = $null -eq $Failure -and $null -ne $StressReport -and $StressReport.passed
  failure = $Failure
  requested_memory_mb = $MemoryMegabytes
  safety_floor_mb = $MinimumAvailableMegabytes
  before = $Before
  under_pressure = $UnderPressure
  after_release = $AfterRelease
  stress_report = $StressReport
}

New-Item -ItemType Directory -Path $TargetDirectory -Force | Out-Null
$Report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $ReportPath -Encoding utf8
$Report |
  Select-Object passed, failure, requested_memory_mb, safety_floor_mb, before, under_pressure, after_release, @{ Name = 'stress_fps'; Expression = { $_.stress_report.fps } }, @{ Name = 'stress_ipc'; Expression = { $_.stress_report.ipc } } |
  ConvertTo-Json -Depth 8
Write-Host "Full report: $ReportPath"

if (-not $Report.passed) {
  exit 1
}
