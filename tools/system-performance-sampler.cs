using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Threading;

namespace GlazeWmDiagnostics
{
    public sealed class SystemPerformanceResult
    {
        public int Samples { get; set; }
        public double MinimumAvailableMegabytes { get; set; }
        public double AveragePageReadsPerSecond { get; set; }
        public double MaximumPageReadsPerSecond { get; set; }
        public double AveragePagesInputPerSecond { get; set; }
        public double MaximumPagesInputPerSecond { get; set; }
        public string Error { get; set; }
    }

    public sealed class SystemPerformanceSampler : IDisposable
    {
        private readonly List<float> _available = new List<float>();
        private readonly List<float> _pageReads = new List<float>();
        private readonly List<float> _pagesInput = new List<float>();
        private Thread _thread;
        private readonly ManualResetEventSlim _ready = new ManualResetEventSlim(false);
        private volatile bool _stopRequested;
        private Exception _error;

        public void Start()
        {
            if (_thread != null)
                throw new InvalidOperationException("System sampler has already started.");
            _thread = new Thread(SampleLoop)
            {
                IsBackground = true,
                Name = "glazewm-system-sampler"
            };
            _thread.Start();
            if (!_ready.Wait(TimeSpan.FromSeconds(10)))
                throw new TimeoutException("System sampler did not become ready within ten seconds.");
            if (_error != null)
                throw new InvalidOperationException("System sampler failed during startup.", _error);
        }

        public SystemPerformanceResult Stop()
        {
            _stopRequested = true;
            if (_thread != null && !_thread.Join(TimeSpan.FromSeconds(5)))
                throw new TimeoutException("System sampler did not stop within five seconds.");
            return new SystemPerformanceResult
            {
                Samples = _available.Count,
                MinimumAvailableMegabytes = _available.Count == 0 ? 0 : _available.Min(),
                AveragePageReadsPerSecond = _pageReads.Count == 0 ? 0 : _pageReads.Average(v => (double)v),
                MaximumPageReadsPerSecond = _pageReads.Count == 0 ? 0 : _pageReads.Max(),
                AveragePagesInputPerSecond = _pagesInput.Count == 0 ? 0 : _pagesInput.Average(v => (double)v),
                MaximumPagesInputPerSecond = _pagesInput.Count == 0 ? 0 : _pagesInput.Max(),
                Error = _error == null ? null : _error.ToString()
            };
        }

        private void SampleLoop()
        {
            try
            {
                using (var available = new PerformanceCounter("Memory", "Available MBytes"))
                using (var pageReads = new PerformanceCounter("Memory", "Page Reads/sec"))
                using (var pagesInput = new PerformanceCounter("Memory", "Pages Input/sec"))
                {
                    pageReads.NextValue();
                    pagesInput.NextValue();
                    while (!_stopRequested)
                    {
                        _available.Add(available.NextValue());
                        _pageReads.Add(pageReads.NextValue());
                        _pagesInput.Add(pagesInput.NextValue());
                        _ready.Set();
                        Thread.Sleep(250);
                    }
                }
            }
            catch (Exception error)
            {
                _error = error;
            }
            finally
            {
                _ready.Set();
            }
        }

        public void Dispose()
        {
            if (_thread != null && _thread.IsAlive)
                Stop();
            _ready.Dispose();
        }
    }
}
