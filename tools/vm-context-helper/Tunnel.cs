// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

using System.Diagnostics;
using System.Runtime.InteropServices;

namespace VmContextHelper;

/// <summary>
/// Owns the `ssh -L` child. A keyed request is only allowed when the listener on
/// the local port belongs to that child; anything else listening there could
/// harvest the bearer token.
/// </summary>
public sealed class Tunnel
{
    public const int Port = 3031;

    readonly Action<string> _log;
    readonly nint _job;
    Process? _child;
    int _backoffMs = 1000;
    DateTime _nextStart = DateTime.MinValue;
    string? _lastNote;

    public Tunnel(Action<string> log)
    {
        _log = log;
        // Kill-on-close: the ssh child dies with this process, however it dies.
        // The helper itself joins the job so every descendant (ProxyCommand etc.)
        // is born inside it, with no assignment gap.
        _job = CreateJobObjectW(0, null);
        if (_job == 0) throw JobFailure("CreateJobObjectW");
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        info.BasicLimitInformation.LimitFlags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if (!SetInformationJobObject(_job, 9 /* ExtendedLimitInformation */, ref info,
                Marshal.SizeOf<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()))
            throw JobFailure("SetInformationJobObject");
        using var self = Process.GetCurrentProcess();
        if (!AssignProcessToJobObject(_job, self.Handle)) throw JobFailure("AssignProcessToJobObject");
    }

    // Names the failing call and its Win32 error; the caller fails closed (no tunnel without the job).
    static InvalidOperationException JobFailure(string call) =>
        new($"job object setup failed: {call} error {Marshal.GetLastWin32Error()}");

    public async Task<bool> EnsureReadyAsync(CancellationToken ct)
    {
        int? child = _child is { HasExited: false } ? _child.Id : null;
        var owners = ListenerOwners();

        if (child is int cid && owners.Count > 0 && owners.All(p => p == cid)) { Note(null); return true; }
        if (owners.Any(p => p != child))
        {
            // Do not spawn or send: someone else holds the port.
            Note($"port {Port} is owned by another process (pid {owners.First(p => p != child)}); not sending");
            return false;
        }
        if (DateTime.UtcNow < _nextStart) return false;

        if (child is null && !Start()) return Fail();
        for (int i = 0; i < 50; i++) // up to 5 s for the forward to come up
        {
            await Task.Delay(100, ct);
            if (_child is not { HasExited: false }) return Fail();
            owners = ListenerOwners();
            if (owners.Count > 0 && owners.All(p => p == _child.Id))
            {
                _backoffMs = 1000;
                Note(null);
                _log("tunnel up");
                return true;
            }
            if (owners.Count > 0) { Note($"port {Port} is owned by another process; not sending"); Kill(); return Fail(); }
        }
        Kill();
        return Fail();
    }

    bool Start()
    {
        try
        {
            var psi = new ProcessStartInfo("ssh")
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardError = true,
            };
            foreach (var a in new[]
            {
                "-N", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=15", "-L", $"127.0.0.1:{Port}:127.0.0.1:3030", "mac",
            }) psi.ArgumentList.Add(a);

            var p = Process.Start(psi) ?? throw new InvalidOperationException("ssh did not start");
            p.EnableRaisingEvents = true;
            p.ErrorDataReceived += (_, e) => { if (!string.IsNullOrWhiteSpace(e.Data)) _log("ssh: " + e.Data); };
            p.Exited += (_, _) => _log("tunnel down");
            p.BeginErrorReadLine();
            _child?.Dispose();
            _child = p;
            return true;
        }
        catch (Exception e)
        {
            _log($"tunnel start failed: {e.GetType().Name}");
            return false;
        }
    }

    void Kill()
    {
        try { _child?.Kill(); } catch (InvalidOperationException) { }
    }

    // Exponential backoff 1 s -> 60 s between start attempts.
    bool Fail()
    {
        _nextStart = DateTime.UtcNow.AddMilliseconds(_backoffMs);
        _backoffMs = Math.Min(_backoffMs * 2, 60_000);
        return false;
    }

    // Log only when the note changes (once per condition).
    void Note(string? msg)
    {
        if (msg == _lastNote) return;
        _lastNote = msg;
        if (msg is not null) _log(msg);
    }

    // ponytail: IPv4 only; we connect to 127.0.0.1, so an IPv6 listener cannot receive our request.
    static HashSet<int> ListenerOwners()
    {
        var set = new HashSet<int>();
        int size = 0;
        GetExtendedTcpTable(0, ref size, false, 2 /* AF_INET */, 3 /* TCP_TABLE_OWNER_PID_LISTENER */, 0);
        var buf = Marshal.AllocHGlobal(size);
        try
        {
            if (GetExtendedTcpTable(buf, ref size, false, 2, 3, 0) != 0) return set;
            int n = Marshal.ReadInt32(buf);
            for (int i = 0; i < n; i++)
            {
                var row = buf + 4 + i * 24; // MIB_TCPROW_OWNER_PID: state, addr, port, rAddr, rPort, pid
                uint addr = (uint)Marshal.ReadInt32(row, 4);
                int rawPort = Marshal.ReadInt32(row, 8);
                int port = ((rawPort & 0xFF) << 8) | ((rawPort >> 8) & 0xFF);
                if (port == Port && (addr == 0x0100007F || addr == 0)) // 127.0.0.1 or wildcard
                    set.Add(Marshal.ReadInt32(row, 20));
            }
        }
        finally { Marshal.FreeHGlobal(buf); }
        return set;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS { public ulong a, b, c, d, e, f; }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public nuint MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public nuint Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public nuint ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [DllImport("kernel32", CharSet = CharSet.Unicode, SetLastError = true)] static extern nint CreateJobObjectW(nint attrs, string? name);
    [DllImport("kernel32", SetLastError = true)] static extern bool SetInformationJobObject(nint job, int cls, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, int size);
    [DllImport("kernel32", SetLastError = true)] static extern bool AssignProcessToJobObject(nint job, nint process);
    [DllImport("iphlpapi")] static extern uint GetExtendedTcpTable(nint table, ref int size, bool sort, int af, int tableClass, uint reserved);
}
