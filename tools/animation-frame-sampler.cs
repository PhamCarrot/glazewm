using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;

namespace GlazeWmDiagnostics
{
    public sealed class FrameSampleResult
    {
        public double DurationSeconds { get; set; }
        public long Samples { get; set; }
        public double CaptureSamplesPerSecond { get; set; }
        public long ChangedFrames { get; set; }
        public long PlausibleFrameIntervals { get; set; }
        public double MedianFrameIntervalMs { get; set; }
        public double P95FrameIntervalMs { get; set; }
        public double EstimatedFps { get; set; }
        public string Error { get; set; }
    }

    public sealed class DesktopFrameSampler : IDisposable
    {
        private const int Srccopy = 0x00CC0020;
        private const int Captureblt = 0x40000000;
        private const uint DibRgbColors = 0;
        private readonly int _requestedWidth;
        private readonly int _requestedHeight;
        private readonly List<long> _changedFrameTicks = new List<long>();
        private Thread _thread;
        private volatile bool _stopRequested;
        private long _sampleCount;
        private long _startTimestamp;
        private long _stopTimestamp;
        private Exception _captureError;

        public DesktopFrameSampler(int width, int height)
        {
            if (width <= 0 || height <= 0)
                throw new ArgumentOutOfRangeException("Capture dimensions must be positive.");
            _requestedWidth = width;
            _requestedHeight = height;
        }

        public void Start()
        {
            if (_thread != null)
                throw new InvalidOperationException("The frame sampler has already started.");

            _stopRequested = false;
            _startTimestamp = Stopwatch.GetTimestamp();
            _thread = new Thread(CaptureLoop)
            {
                IsBackground = true,
                Name = "glazewm-frame-sampler",
                Priority = ThreadPriority.AboveNormal
            };
            _thread.Start();
        }

        public FrameSampleResult Stop()
        {
            _stopRequested = true;
            if (_thread != null && !_thread.Join(TimeSpan.FromSeconds(5)))
                throw new TimeoutException("Desktop frame sampler did not stop within five seconds.");
            _stopTimestamp = Stopwatch.GetTimestamp();
            return BuildResult();
        }

        private void CaptureLoop()
        {
            IntPtr screenDc = IntPtr.Zero;
            IntPtr memoryDc = IntPtr.Zero;
            IntPtr dib = IntPtr.Zero;
            IntPtr oldObject = IntPtr.Zero;

            try
            {
                int screenWidth = GetSystemMetrics(0);
                int screenHeight = GetSystemMetrics(1);
                int width = Math.Min(_requestedWidth, screenWidth);
                int height = Math.Min(_requestedHeight, screenHeight);
                int sourceX = Math.Max(0, (screenWidth - width) / 2);
                int sourceY = Math.Max(0, (screenHeight - height) / 2);

                screenDc = GetDC(IntPtr.Zero);
                if (screenDc == IntPtr.Zero)
                    throw new InvalidOperationException("GetDC failed.");
                memoryDc = CreateCompatibleDC(screenDc);
                if (memoryDc == IntPtr.Zero)
                    throw new InvalidOperationException("CreateCompatibleDC failed.");

                var info = new BitmapInfo
                {
                    Header = new BitmapInfoHeader
                    {
                        Size = (uint)Marshal.SizeOf(typeof(BitmapInfoHeader)),
                        Width = width,
                        Height = -height,
                        Planes = 1,
                        BitCount = 32,
                        Compression = 0
                    }
                };
                IntPtr pixelData;
                dib = CreateDIBSection(memoryDc, ref info, DibRgbColors, out pixelData, IntPtr.Zero, 0);
                if (dib == IntPtr.Zero || pixelData == IntPtr.Zero)
                    throw new InvalidOperationException("CreateDIBSection failed.");
                oldObject = SelectObject(memoryDc, dib);

                var pixels = new byte[checked(width * height * 4)];
                ulong previousHash = 0;
                bool hasPreviousHash = false;

                while (!_stopRequested)
                {
                    if (!BitBlt(memoryDc, 0, 0, width, height, screenDc, sourceX, sourceY, Srccopy | Captureblt))
                        throw new InvalidOperationException("BitBlt failed.");

                    Marshal.Copy(pixelData, pixels, 0, pixels.Length);
                    ulong hash = 1469598103934665603UL;
                    // Sample every fourth pixel. This is dense enough to detect
                    // moving window edges while leaving headroom above 144 Hz.
                    for (int i = 0; i < pixels.Length; i += 16)
                    {
                        hash ^= pixels[i];
                        hash *= 1099511628211UL;
                        hash ^= pixels[i + 1];
                        hash *= 1099511628211UL;
                        hash ^= pixels[i + 2];
                        hash *= 1099511628211UL;
                    }

                    Interlocked.Increment(ref _sampleCount);
                    if (!hasPreviousHash || hash != previousHash)
                    {
                        _changedFrameTicks.Add(Stopwatch.GetTimestamp());
                        previousHash = hash;
                        hasPreviousHash = true;
                    }
                    Thread.Yield();
                }
            }
            catch (Exception error)
            {
                _captureError = error;
            }
            finally
            {
                if (oldObject != IntPtr.Zero && memoryDc != IntPtr.Zero)
                    SelectObject(memoryDc, oldObject);
                if (dib != IntPtr.Zero)
                    DeleteObject(dib);
                if (memoryDc != IntPtr.Zero)
                    DeleteDC(memoryDc);
                if (screenDc != IntPtr.Zero)
                    ReleaseDC(IntPtr.Zero, screenDc);
            }
        }

        private FrameSampleResult BuildResult()
        {
            double durationSeconds = Math.Max(
                0.000001,
                (_stopTimestamp - _startTimestamp) / (double)Stopwatch.Frequency);
            var intervals = new List<double>();
            for (int i = 1; i < _changedFrameTicks.Count; i++)
            {
                double milliseconds = (_changedFrameTicks[i] - _changedFrameTicks[i - 1])
                    * 1000.0 / Stopwatch.Frequency;
                // Exclude sub-vblank noise and idle/command gaps. At the
                // active 143 Hz mode a real frame is about 6.99 ms.
                if (milliseconds >= 3.0 && milliseconds <= 25.0)
                    intervals.Add(milliseconds);
            }
            intervals.Sort();

            double median = Percentile(intervals, 0.50);
            double p95 = Percentile(intervals, 0.95);
            return new FrameSampleResult
            {
                DurationSeconds = durationSeconds,
                Samples = _sampleCount,
                CaptureSamplesPerSecond = _sampleCount / durationSeconds,
                ChangedFrames = _changedFrameTicks.Count,
                PlausibleFrameIntervals = intervals.Count,
                MedianFrameIntervalMs = median,
                P95FrameIntervalMs = p95,
                EstimatedFps = median > 0.0 ? 1000.0 / median : 0.0,
                Error = _captureError == null ? null : _captureError.ToString()
            };
        }

        private static double Percentile(List<double> sortedValues, double percentile)
        {
            if (sortedValues.Count == 0)
                return 0.0;
            int index = Math.Min(
                sortedValues.Count - 1,
                Math.Max(0, (int)Math.Ceiling(sortedValues.Count * percentile) - 1));
            return sortedValues[index];
        }

        public void Dispose()
        {
            if (_thread != null && _thread.IsAlive)
                Stop();
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct BitmapInfoHeader
        {
            public uint Size;
            public int Width;
            public int Height;
            public ushort Planes;
            public ushort BitCount;
            public uint Compression;
            public uint SizeImage;
            public int XPelsPerMeter;
            public int YPelsPerMeter;
            public uint ClrUsed;
            public uint ClrImportant;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct BitmapInfo
        {
            public BitmapInfoHeader Header;
            public uint Colors;
        }

        [DllImport("user32.dll")]
        private static extern IntPtr GetDC(IntPtr window);

        [DllImport("user32.dll")]
        private static extern int ReleaseDC(IntPtr window, IntPtr dc);

        [DllImport("user32.dll")]
        private static extern int GetSystemMetrics(int index);

        [DllImport("gdi32.dll")]
        private static extern IntPtr CreateCompatibleDC(IntPtr dc);

        [DllImport("gdi32.dll")]
        private static extern bool DeleteDC(IntPtr dc);

        [DllImport("gdi32.dll")]
        private static extern IntPtr CreateDIBSection(
            IntPtr dc,
            ref BitmapInfo info,
            uint usage,
            out IntPtr bits,
            IntPtr section,
            uint offset);

        [DllImport("gdi32.dll")]
        private static extern IntPtr SelectObject(IntPtr dc, IntPtr value);

        [DllImport("gdi32.dll")]
        private static extern bool DeleteObject(IntPtr value);

        [DllImport("gdi32.dll")]
        private static extern bool BitBlt(
            IntPtr destination,
            int x,
            int y,
            int width,
            int height,
            IntPtr source,
            int sourceX,
            int sourceY,
            int operation);
    }
}
