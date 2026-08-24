[CmdletBinding()]
param(
  [ValidateRange(2, 20)]
  [int]$WindowCount = 8,

  [ValidateRange(1, 20)]
  [int]$Cycles = 3,

  [ValidateRange(10, 1000)]
  [int]$DelayMs = 45,

  [string]$CliPath
)

$ErrorActionPreference = 'Stop'
$CliPath = if ($CliPath) {
  $CliPath
} else {
  Join-Path $PSScriptRoot '..\out\glazewm-combined-v0.0.0-windows-x64\cli\glazewm.exe'
}
$CliPath = [IO.Path]::GetFullPath($CliPath)
$WindowPath = Join-Path $PSScriptRoot 'animation-stress-window.hta'
$FrameSamplerPath = Join-Path $PSScriptRoot 'animation-frame-sampler.cs'
$IpcClientPath = Join-Path $PSScriptRoot 'persistent-ipc-client.cs'
$SystemSamplerPath = Join-Path $PSScriptRoot 'system-performance-sampler.cs'
$RunId = Get-Date -Format 'yyyyMMdd-HHmmss'
$ReportPath = Join-Path $PSScriptRoot "..\target\animation-stress-report-$RunId.json"
$Layouts = @(
  'bsp',
  'split_horizontal',
  'split_vertical',
  'columns',
  'rows',
  'grid',
  'vertical_stack',
  'horizontal_stack'
)
$CommandSamples = [Collections.Generic.List[object]]::new()
$SpawnedProcesses = [Collections.Generic.List[Diagnostics.Process]]::new()
$NewWindowIds = [Collections.Generic.List[string]]::new()
$OriginalWorkspace = $null
$OriginalLayout = $null
$OriginalFocusId = $null
$TargetWorkspace = $null
$TargetOriginalLayout = $null
$WmProcess = $null
$PeakWorkingSetBytes = 0L
$CpuSecondsBefore = 0.0
$TestStopwatch = [Diagnostics.Stopwatch]::StartNew()
$CleanupErrors = [Collections.Generic.List[string]]::new()
$FrameSampler = $null
$FrameSamplerStarted = $false
$FrameStats = $null
$IpcClient = $null
$SystemSampler = $null
$SystemSamplerStarted = $false
$SystemStats = $null

function Get-WmProcess {
  $processes = @(Get-Process -Name glazewm -ErrorAction SilentlyContinue)
  if ($processes.Count -ne 1) {
    throw "Expected exactly one running glazewm process; found $($processes.Count)."
  }
  return $processes[0]
}

function Update-WmResourcePeak {
  if ($null -eq $script:WmProcess) {
    return
  }
  try {
    $script:WmProcess.Refresh()
    if ($script:WmProcess.WorkingSet64 -gt $script:PeakWorkingSetBytes) {
      $script:PeakWorkingSetBytes = $script:WmProcess.WorkingSet64
    }
  } catch {
    throw 'The GlazeWM process exited during the stress run.'
  }
}

function Invoke-GlazeCommand {
  param(
    [Parameter(Mandatory)]
    [string[]]$Arguments,

    [switch]$AllowFailure
  )

  $stopwatch = [Diagnostics.Stopwatch]::StartNew()
  try {
    $output = $script:IpcClient.RoundTrip($Arguments -join ' ')
    $exitCode = 0
  } catch {
    $output = $_.Exception.ToString()
    $exitCode = 1
  }
  $stopwatch.Stop()

  $response = $null
  if ($output) {
    try {
      $response = $output | ConvertFrom-Json
    } catch {
      $response = $null
    }
  }

  $success = $exitCode -eq 0 -and
    ($null -eq $response -or $null -eq $response.success -or $response.success)
  [void]$script:CommandSamples.Add([pscustomobject]@{
    command = $Arguments -join ' '
    elapsed_ms = [Math]::Round($stopwatch.Elapsed.TotalMilliseconds, 3)
    success = $success
    exit_code = $exitCode
    error = if ($null -ne $response -and $response.error) { $response.error } elseif (-not $success) { $output } else { $null }
  })
  Update-WmResourcePeak

  if (-not $success -and -not $AllowFailure) {
    throw "GlazeWM command failed: $($Arguments -join ' ')`n$output"
  }
  return $response
}

function Get-WindowSnapshot {
  $response = Invoke-GlazeCommand -Arguments @('query', 'windows')
  return @($response.data.windows)
}

function Wait-ForStressWindows {
  param(
    [string[]]$BaselineIds,
    [int]$ExpectedCount
  )

  $deadline = [DateTime]::UtcNow.AddSeconds(12)
  do {
    $windows = Get-WindowSnapshot
    $newWindows = @($windows | Where-Object {
      $_.processName -eq 'mshta' -and $_.id -notin $BaselineIds
    })
    if ($newWindows.Count -ge $ExpectedCount) {
      return $newWindows
    }
    Start-Sleep -Milliseconds 100
  } while ([DateTime]::UtcNow -lt $deadline)

  return $newWindows
}

function Restore-OriginalState {
  if ($script:TargetWorkspace -and $script:TargetOriginalLayout) {
    try {
      [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--workspace', $script:TargetWorkspace) -AllowFailure)
      [void](Invoke-GlazeCommand -Arguments @('command', 'set-workspace-layout', $script:TargetOriginalLayout) -AllowFailure)
    } catch {
      [void]$script:CleanupErrors.Add("Could not restore target workspace layout: $($_.Exception.Message)")
    }
  }

  if ($script:OriginalWorkspace) {
    try {
      [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--workspace', $script:OriginalWorkspace) -AllowFailure)
    } catch {
      [void]$script:CleanupErrors.Add("Could not restore original workspace: $($_.Exception.Message)")
    }
  }

  if ($script:OriginalFocusId) {
    try {
      [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--container-id', $script:OriginalFocusId) -AllowFailure)
    } catch {
      [void]$script:CleanupErrors.Add("Could not restore original focus: $($_.Exception.Message)")
    }
  }
}

if (-not (Test-Path -LiteralPath $CliPath -PathType Leaf)) {
  throw "GlazeWM CLI not found: $CliPath"
}
if (-not (Test-Path -LiteralPath $WindowPath -PathType Leaf)) {
  throw "Stress-window HTA not found: $WindowPath"
}
if (-not (Test-Path -LiteralPath $FrameSamplerPath -PathType Leaf)) {
  throw "Frame sampler source not found: $FrameSamplerPath"
}
if (-not (Test-Path -LiteralPath $IpcClientPath -PathType Leaf)) {
  throw "Persistent IPC client source not found: $IpcClientPath"
}
if (-not (Test-Path -LiteralPath $SystemSamplerPath -PathType Leaf)) {
  throw "System-performance sampler source not found: $SystemSamplerPath"
}
Add-Type -Path $FrameSamplerPath
Add-Type -Path $IpcClientPath
Add-Type -Path $SystemSamplerPath
$IpcClient = [GlazeWmDiagnostics.PersistentIpcClient]::new('ws://127.0.0.1:6123')
$IpcClient.Connect()

$WmProcess = Get-WmProcess
$WmProcess.Refresh()
$WmProcessId = $WmProcess.Id
$CpuSecondsBefore = $WmProcess.TotalProcessorTime.TotalSeconds
$WorkingSetBefore = $WmProcess.WorkingSet64
$PeakWorkingSetBytes = $WorkingSetBefore
$BaselineWindows = Get-WindowSnapshot
$BaselineIds = @($BaselineWindows.id)
$FocusedResponse = Invoke-GlazeCommand -Arguments @('query', 'focused')
$LayoutResponse = Invoke-GlazeCommand -Arguments @('query', 'layout')
$WorkspacesResponse = Invoke-GlazeCommand -Arguments @('query', 'workspaces')
$PausedResponse = Invoke-GlazeCommand -Arguments @('query', 'paused')

if ($PausedResponse.data -eq $true) {
  throw 'GlazeWM is paused. Resume it before running the animation stress test.'
}

$OriginalFocusId = $FocusedResponse.data.focused.id
$OriginalLayout = $LayoutResponse.data.layout
$OriginalWorkspaceObject = @($WorkspacesResponse.data.workspaces | Where-Object hasFocus)[0]
if ($null -eq $OriginalWorkspaceObject) {
  throw 'Could not determine the currently focused workspace.'
}
$OriginalWorkspace = $OriginalWorkspaceObject.name
$TargetWorkspaceObject = @($WorkspacesResponse.data.workspaces | Where-Object {
  -not $_.hasFocus -and @($_.children).Count -eq 0
})[0]
if ($null -eq $TargetWorkspaceObject) {
  throw 'No empty inactive workspace is available for an isolated stress test.'
}
$TargetWorkspace = $TargetWorkspaceObject.name
$TargetOriginalLayout = $TargetWorkspaceObject.layout

$Failure = $null
try {
  $FrameSampler = [GlazeWmDiagnostics.DesktopFrameSampler]::new(320, 180)
  $FrameSampler.Start()
  $FrameSamplerStarted = $true
  $SystemSampler = [GlazeWmDiagnostics.SystemPerformanceSampler]::new()
  $SystemSampler.Start()
  $SystemSamplerStarted = $true
  [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--workspace', $TargetWorkspace))
  Start-Sleep -Milliseconds ([Math]::Max(300, $DelayMs))

  for ($i = 1; $i -le $WindowCount; $i++) {
    $process = Start-Process -FilePath "$env:WINDIR\System32\mshta.exe" -ArgumentList ('"{0}"' -f $WindowPath) -PassThru
    [void]$SpawnedProcesses.Add($process)
    Start-Sleep -Milliseconds 25
  }

  $StressWindows = @(Wait-ForStressWindows -BaselineIds $BaselineIds -ExpectedCount $WindowCount)
  if ($StressWindows.Count -lt $WindowCount) {
    throw "Only $($StressWindows.Count) of $WindowCount stress windows were managed within 12 seconds."
  }
  foreach ($window in $StressWindows) {
    [void]$NewWindowIds.Add([string]$window.id)
  }

  for ($cycle = 1; $cycle -le $Cycles; $cycle++) {
    foreach ($layout in $Layouts) {
      [void](Invoke-GlazeCommand -Arguments @('command', 'set-workspace-layout', $layout))
      Start-Sleep -Milliseconds $DelayMs
    }

    foreach ($id in $NewWindowIds) {
      [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--container-id', $id))
      Start-Sleep -Milliseconds $DelayMs
    }

    $resizeId = $NewWindowIds[$cycle % $NewWindowIds.Count]
    [void](Invoke-GlazeCommand -Arguments @('command', 'set-workspace-layout', 'split_horizontal'))
    [void](Invoke-GlazeCommand -Arguments @('command', '--id', $resizeId, 'resize', '--width', '+8%'))
    Start-Sleep -Milliseconds $DelayMs
    [void](Invoke-GlazeCommand -Arguments @('command', '--id', $resizeId, 'resize', '--width', '-8%'))

    $stateId = $NewWindowIds[($cycle + 1) % $NewWindowIds.Count]
    [void](Invoke-GlazeCommand -Arguments @('command', '--id', $stateId, 'set-floating', '--centered'))
    Start-Sleep -Milliseconds $DelayMs
    [void](Invoke-GlazeCommand -Arguments @('command', '--id', $stateId, 'set-tiling'))

    [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--workspace', $OriginalWorkspace))
    Start-Sleep -Milliseconds $DelayMs
    [void](Invoke-GlazeCommand -Arguments @('command', 'focus', '--workspace', $TargetWorkspace))
    Start-Sleep -Milliseconds $DelayMs
  }

  [void](Invoke-GlazeCommand -Arguments @('command', 'set-workspace-layout', $TargetOriginalLayout))
  foreach ($id in @($NewWindowIds)) {
    [void](Invoke-GlazeCommand -Arguments @('command', '--id', $id, 'close') -AllowFailure)
    Start-Sleep -Milliseconds $DelayMs
  }
  Start-Sleep -Milliseconds 500
} catch {
  $Failure = $_.Exception.Message
} finally {
  if ($FrameSamplerStarted) {
    try {
      $FrameStats = $FrameSampler.Stop()
    } catch {
      [void]$CleanupErrors.Add("Could not stop frame sampler: $($_.Exception.Message)")
    }
  }
  if ($SystemSamplerStarted) {
    try {
      $SystemStats = $SystemSampler.Stop()
    } catch {
      [void]$CleanupErrors.Add("Could not stop system sampler: $($_.Exception.Message)")
    }
  }
  foreach ($process in $SpawnedProcesses) {
    try {
      if (-not $process.HasExited) {
        $process.CloseMainWindow() | Out-Null
        if (-not $process.WaitForExit(1500)) {
          Stop-Process -Id $process.Id -Force -ErrorAction Stop
        }
      }
    } catch {
      [void]$CleanupErrors.Add("Could not stop helper process $($process.Id): $($_.Exception.Message)")
    }
  }
  Restore-OriginalState
  if ($null -ne $IpcClient) {
    $IpcClient.Dispose()
    $IpcClient = $null
  }
  $TestStopwatch.Stop()
}

$WmAlive = $null -ne (Get-Process -Id $WmProcessId -ErrorAction SilentlyContinue)
$CpuSecondsAfter = $CpuSecondsBefore
$WorkingSetAfter = 0L
if ($WmAlive) {
  $WmProcess.Refresh()
  $CpuSecondsAfter = $WmProcess.TotalProcessorTime.TotalSeconds
  $WorkingSetAfter = $WmProcess.WorkingSet64
}

$Latencies = @($CommandSamples | ForEach-Object elapsed_ms | Sort-Object)
$P95Index = if ($Latencies.Count -gt 0) {
  [Math]::Min($Latencies.Count - 1, [Math]::Ceiling($Latencies.Count * 0.95) - 1)
} else { 0 }
$FailedCommands = @($CommandSamples | Where-Object { -not $_.success })
# The FPS estimate is inferred from intervals between changed captures and
# can legitimately exceed the screenshot sampling rate when intermediate
# frames are missed. Validate sampler health and usable motion evidence
# directly instead of comparing those two different rates.
$FrameMeasurementPassed = $null -ne $FrameStats -and
  -not $FrameStats.Error -and
  $FrameStats.EstimatedFps -gt 0 -and
  $FrameStats.CaptureSamplesPerSecond -ge 30 -and
  $FrameStats.ChangedFrames -ge 2 -and
  $FrameStats.PlausibleFrameIntervals -ge 1
$Report = [ordered]@{
  run_id = $RunId
  passed = $null -eq $Failure -and
    $WmAlive -and
    $FailedCommands.Count -eq 0 -and
    $CleanupErrors.Count -eq 0 -and
    $FrameMeasurementPassed
  failure = $Failure
  settings = [ordered]@{
    window_count = $WindowCount
    cycles = $Cycles
    delay_ms = $DelayMs
    layouts = $Layouts
    command_transport = 'persistent_websocket'
  }
  isolation = [ordered]@{
    original_workspace = $OriginalWorkspace
    original_layout = $OriginalLayout
    original_focus_id = $OriginalFocusId
    target_workspace = $TargetWorkspace
    target_original_layout = $TargetOriginalLayout
    managed_stress_windows = $NewWindowIds.Count
  }
  stability = [ordered]@{
    wm_process_id = $WmProcessId
    wm_alive_after_test = $WmAlive
    elapsed_seconds = [Math]::Round($TestStopwatch.Elapsed.TotalSeconds, 3)
    wm_cpu_seconds = [Math]::Round($CpuSecondsAfter - $CpuSecondsBefore, 3)
    wm_working_set_before_mb = [Math]::Round($WorkingSetBefore / 1MB, 3)
    wm_working_set_peak_mb = [Math]::Round($PeakWorkingSetBytes / 1MB, 3)
    wm_working_set_after_mb = [Math]::Round($WorkingSetAfter / 1MB, 3)
  }
  ipc = [ordered]@{
    command_count = $CommandSamples.Count
    failed_count = $FailedCommands.Count
    average_ms = if ($Latencies.Count) { [Math]::Round(($Latencies | Measure-Object -Average).Average, 3) } else { 0 }
    p95_ms = if ($Latencies.Count) { $Latencies[$P95Index] } else { 0 }
    maximum_ms = if ($Latencies.Count) { $Latencies[-1] } else { 0 }
  }
  fps = if ($FrameStats) {
    [ordered]@{
      estimated_fps = [Math]::Round($FrameStats.EstimatedFps, 3)
      median_frame_interval_ms = [Math]::Round($FrameStats.MedianFrameIntervalMs, 3)
      p95_frame_interval_ms = [Math]::Round($FrameStats.P95FrameIntervalMs, 3)
      changed_frames = $FrameStats.ChangedFrames
      plausible_frame_intervals = $FrameStats.PlausibleFrameIntervals
      capture_samples = $FrameStats.Samples
      capture_samples_per_second = [Math]::Round($FrameStats.CaptureSamplesPerSecond, 3)
      duration_seconds = [Math]::Round($FrameStats.DurationSeconds, 3)
      error = $FrameStats.Error
    }
  } else { $null }
  system_performance = if ($SystemStats) {
    [ordered]@{
      samples = $SystemStats.Samples
      minimum_available_mb = [Math]::Round($SystemStats.MinimumAvailableMegabytes, 3)
      average_page_reads_per_second = [Math]::Round($SystemStats.AveragePageReadsPerSecond, 3)
      maximum_page_reads_per_second = [Math]::Round($SystemStats.MaximumPageReadsPerSecond, 3)
      average_pages_input_per_second = [Math]::Round($SystemStats.AveragePagesInputPerSecond, 3)
      maximum_pages_input_per_second = [Math]::Round($SystemStats.MaximumPagesInputPerSecond, 3)
      error = $SystemStats.Error
    }
  } else { $null }
  cleanup_errors = @($CleanupErrors)
  failed_commands = $FailedCommands
  command_samples = $CommandSamples
}

$ReportDirectory = Split-Path -Parent $ReportPath
New-Item -ItemType Directory -Path $ReportDirectory -Force | Out-Null
$ReportObject = [pscustomobject]$Report
$ReportObject | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $ReportPath -Encoding utf8
$ReportObject |
  Select-Object passed, failure, settings, isolation, stability, ipc, fps, system_performance, cleanup_errors |
  ConvertTo-Json -Depth 8
Write-Host "Full report: $([IO.Path]::GetFullPath($ReportPath))"

if (-not $ReportObject.passed) {
  exit 1
}
