// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

using System.Runtime.InteropServices;
using VmContextHelper;

// Reports the foreground Windows app/title to the screenpipe host through an
// ssh tunnel. Records and stores nothing itself; the log holds app names only.
static class Program
{
    [DllImport("kernel32")] static extern bool AttachConsole(int pid);

    [STAThread]
    static int Main(string[] args)
    {
        if (args.Contains("--once"))
        {
            AttachConsole(-1); // WinExe: borrow the parent's console so output is visible
            var info = Foreground.Read();
            Console.WriteLine(info is null ? "(no foreground window)" : $"{info.AppName} | {info.WindowTitle}");
            return 0;
        }

        var keyPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "vm-context-helper", "api-key");
        // The key is never logged; only a missing/unreadable file is reported (by exception type).
        string ReadKey() => File.ReadAllText(keyPath).Trim();

        Tunnel tunnel;
        try { tunnel = new Tunnel(Log.Write); }
        catch (InvalidOperationException e)
        {
            // Fail closed: without the kill-on-close job we do not run at all.
            Log.Write(e.Message);
            return 1;
        }
        var epoch = (ulong)DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        bool keyMissingLogged = false;
        // No key file: neither start the tunnel nor send; rechecked on each retry/heartbeat.
        Task<bool> Ready(CancellationToken ct)
        {
            if (!File.Exists(keyPath))
            {
                if (!keyMissingLogged) Log.Write(@"API key file missing: %APPDATA%\vm-context-helper\api-key");
                keyMissingLogged = true;
                return Task.FromResult(false);
            }
            keyMissingLogged = false;
            return tunnel.EnsureReadyAsync(ct);
        }
        var sender = new Sender(new Uri($"http://127.0.0.1:{Tunnel.Port}/"), ReadKey, epoch, Ready, Log.Write);
        _ = Task.Run(() => sender.RunAsync(CancellationToken.None));

        bool locked = false;
        string? lastApp = null, lastKey = null;

        void Publish(bool force)
        {
            var info = Foreground.Read();
            if (info is null) return; // transient/unreadable: keep the previous state
            var key = info.AppName + "\0" + info.WindowTitle;
            if (!force && key == lastKey) return;
            lastKey = key;
            if (info.AppName != lastApp) { lastApp = info.AppName; Log.Write($"foreground: {info.AppName}"); }
            sender.Publish(info);
        }

        Publish(true);
        Foreground.Run(
            onForegroundEvent: () => { if (!locked) Publish(false); },
            onLock: () => { locked = true; Log.Write("session locked"); sender.Publish(null); sender.Pause(); lastKey = null; },
            onUnlock: () => { locked = false; Log.Write("session unlocked"); sender.Resume(); Publish(true); });
        return 0;
    }
}

static class Log
{
    const long MaxBytes = 1_000_000;
    static readonly object Gate = new();
    static readonly string Path_ = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "vm-context-helper", "helper.log");

    public static void Write(string line)
    {
        lock (Gate)
        {
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(Path_)!);
                var f = new FileInfo(Path_);
                if (f.Exists && f.Length > MaxBytes) File.Move(Path_, Path_ + ".1", overwrite: true);
                File.AppendAllText(Path_, $"{DateTime.Now:s} {line}{Environment.NewLine}");
            }
            catch (IOException) { } // logging must never take the helper down
            catch (UnauthorizedAccessException) { }
        }
    }
}
