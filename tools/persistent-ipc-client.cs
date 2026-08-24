using System;
using System.IO;
using System.Net.WebSockets;
using System.Text;
using System.Threading;

namespace GlazeWmDiagnostics
{
    public sealed class PersistentIpcClient : IDisposable
    {
        private readonly ClientWebSocket _socket = new ClientWebSocket();
        private readonly Uri _endpoint;

        public PersistentIpcClient(string endpoint)
        {
            _endpoint = new Uri(endpoint);
            _socket.Options.KeepAliveInterval = TimeSpan.FromSeconds(10);
        }

        public void Connect()
        {
            using (var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(5)))
                _socket.ConnectAsync(_endpoint, timeout.Token).GetAwaiter().GetResult();
        }

        public string RoundTrip(string message)
        {
            if (_socket.State != WebSocketState.Open)
                throw new InvalidOperationException("GlazeWM IPC socket is not open.");

            byte[] request = Encoding.UTF8.GetBytes(message);
            using (var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10)))
            {
                _socket.SendAsync(
                    new ArraySegment<byte>(request),
                    WebSocketMessageType.Text,
                    true,
                    timeout.Token).GetAwaiter().GetResult();

                using (var response = new MemoryStream())
                {
                    var buffer = new byte[16384];
                    WebSocketReceiveResult result;
                    do
                    {
                        result = _socket.ReceiveAsync(
                            new ArraySegment<byte>(buffer),
                            timeout.Token).GetAwaiter().GetResult();
                        if (result.MessageType == WebSocketMessageType.Close)
                            throw new IOException("GlazeWM IPC socket closed while waiting for a response.");
                        response.Write(buffer, 0, result.Count);
                    }
                    while (!result.EndOfMessage);

                    return Encoding.UTF8.GetString(response.ToArray());
                }
            }
        }

        public void Dispose()
        {
            if (_socket.State == WebSocketState.Open)
            {
                try
                {
                    using (var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(1)))
                        _socket.CloseAsync(
                            WebSocketCloseStatus.NormalClosure,
                            "diagnostic complete",
                            timeout.Token).GetAwaiter().GetResult();
                }
                catch
                {
                    _socket.Abort();
                }
            }
            _socket.Dispose();
        }
    }
}
