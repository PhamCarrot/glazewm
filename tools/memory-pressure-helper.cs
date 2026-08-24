using System;
using System.Collections.Generic;

namespace GlazeWmDiagnostics
{
    /// <summary>
    /// Holds a bounded amount of committed, page-touched memory for a
    /// diagnostic run. Dispose releases only memory owned by this instance.
    /// </summary>
    public sealed class MemoryPressureLease : IDisposable
    {
        private const int ChunkMegabytes = 64;
        private readonly List<byte[]> _chunks = new List<byte[]>();

        public MemoryPressureLease(int targetMegabytes)
        {
            if (targetMegabytes <= 0)
                throw new ArgumentOutOfRangeException("targetMegabytes");

            try
            {
                int remaining = targetMegabytes;
                while (remaining > 0)
                {
                    int chunkMegabytes = Math.Min(ChunkMegabytes, remaining);
                    var chunk = new byte[checked(chunkMegabytes * 1024 * 1024)];
                    // Touch every page so this is physical-memory pressure,
                    // not merely reserved virtual address space.
                    for (int offset = 0; offset < chunk.Length; offset += 4096)
                        chunk[offset] = 0x5A;
                    chunk[chunk.Length - 1] = 0xA5;
                    _chunks.Add(chunk);
                    AllocatedMegabytes += chunkMegabytes;
                    remaining -= chunkMegabytes;
                }
            }
            catch
            {
                Dispose();
                throw;
            }
        }

        public int AllocatedMegabytes { get; private set; }

        public void Dispose()
        {
            _chunks.Clear();
            AllocatedMegabytes = 0;
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
        }
    }
}
