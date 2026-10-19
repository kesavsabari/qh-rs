using System.Diagnostics;
using System.Net.Sockets;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;

namespace Qh;

/// The line to `qh serve` for one project, like apple/qh/ServeClient.swift: starts it (or finds the
/// one already running), reads its token, connects to its AF_UNIX socket, and hands every event line
/// to OnEvent on the UI thread. Views never touch this; Project turns the events into state.
sealed class ServeClient(string path, string qh, DispatcherQueue ui)
{
    Process? proc;
    Socket? sock;
    readonly object writeLock = new();
    public Action<JsonObject>? OnEvent;
    public Action<string>? OnClose;

    /// Off the UI thread: launch or find the server, then read until the socket closes.
    public void Start() => new Thread(() =>
    {
        try
        {
            var (where, token) = Launch();
            sock = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
            sock.Connect(new UnixDomainSocketEndPoint(where));
            Send(new JsonObject { ["type"] = "hello", ["token"] = token });
            ReadLoop();
            Close("qh serve stopped");
        }
        catch (Exception e) { Close(e.Message); }
    }) { IsBackground = true }.Start();

    /// Its first stdout line says where to connect ("listening" = we started it, "running" = it already was).
    (string, string) Launch()
    {
        var log = Path.Combine(Path.GetTempPath(), $"qh-serve-{(uint)path.GetHashCode()}.log");
        var p = new Process
        {
            StartInfo = new ProcessStartInfo(qh, "serve")
            {
                WorkingDirectory = path,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                RedirectStandardInput = true,
                UseShellExecute = false,
                CreateNoWindow = true,
                StandardOutputEncoding = Encoding.UTF8,
            }
        };
        p.StartInfo.Environment["QH_EDITOR"] = "desktop";
        p.Start();
        // stderr to a file: nobody reads a pipe after startup and a full one would stall qh
        var errLog = new StreamWriter(log) { AutoFlush = true };
        p.ErrorDataReceived += (_, e) => { if (e.Data != null) lock (errLog) errLog.WriteLine(e.Data); };
        p.BeginErrorReadLine();
        while (p.StandardOutput.ReadLine() is { } line)
        {
            if (JsonNode.Parse(line) is not JsonObject j) continue;
            var type = (string?)j["type"];
            if (type is not ("listening" or "running")) continue;
            if (type == "listening") proc = p;
            var token = File.ReadAllText((string)j["token_file"]!).Trim();
            return ((string)j["socket"]!, token);
        }
        p.WaitForExit();
        lock (errLog) errLog.Flush();
        var msg = File.Exists(log) ? File.ReadAllText(log).Trim() : "";
        throw new Exception(msg.Length == 0 ? "qh serve didn't start" : msg);
    }

    void ReadLoop()
    {
        using var r = new StreamReader(new NetworkStream(sock!, ownsSocket: false), Encoding.UTF8);
        while (r.ReadLine() is { } line)
        {
            JsonObject? j;
            try { j = JsonNode.Parse(line) as JsonObject; } catch { continue; }
            if (j != null) ui.TryEnqueue(() => OnEvent?.Invoke(j));
        }
    }

    public void Send(JsonObject msg)
    {
        var s = sock;
        if (s == null) return;
        var bytes = Encoding.UTF8.GetBytes(msg.ToJsonString() + "\n");
        lock (writeLock) { try { s.Send(bytes); } catch (SocketException) { } }
    }

    void Close(string why)
    {
        try { sock?.Close(); } catch { }
        sock = null;
        ui.TryEnqueue(() => OnClose?.Invoke(why));
    }

    /// Ends the server only if this app started it; one you started yourself keeps running.
    public void Stop()
    {
        try { sock?.Shutdown(SocketShutdown.Both); } catch { }
        try { if (proc is { HasExited: false }) proc.Kill(); } catch { }
    }
}
