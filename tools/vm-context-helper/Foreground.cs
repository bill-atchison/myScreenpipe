// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

using System.Runtime.InteropServices;
using System.Text;

namespace VmContextHelper;

public static class Foreground
{
    // Same list as screenpipe-a11y TRANSIENT_SHELL_WINDOW_CLASSES: windows that
    // briefly steal focus on clicks and say nothing about what the user is in.
    static readonly string[] TransientClasses =
    {
        "MSCTFIME UI", "CiceroUIWndFrame", "Shell_TrayWnd", "tooltips_class32",
        "TopLevelWindowForOverflowXamlIsland",
    };

    const string FrameHost = "ApplicationFrameHost";

    /// <summary>"C:\x\Foo.exe" -> "Foo"; UWP apps live behind ApplicationFrameHost, so use the hosted app's name.</summary>
    public static string NormalizeAppName(string imagePath, string? uwpAppName)
    {
        var stem = Path.GetFileNameWithoutExtension(imagePath);
        if (stem.Equals(FrameHost, StringComparison.OrdinalIgnoreCase) && !string.IsNullOrEmpty(uwpAppName))
            return uwpAppName;
        return stem;
    }

    /// <summary>null = no usable foreground window (none, transient shell window, unreadable process).</summary>
    public static ForegroundInfo? Read()
    {
        var hwnd = GetForegroundWindow();
        if (hwnd == 0 || IsTransient(hwnd)) return null;
        GetWindowThreadProcessId(hwnd, out uint pid);
        var image = ImagePath(pid);
        if (image is null) return null;

        string? uwp = null;
        if (Path.GetFileNameWithoutExtension(image).Equals(FrameHost, StringComparison.OrdinalIgnoreCase))
            uwp = HostedAppName(hwnd, pid);

        var buf = new StringBuilder(2048);
        GetWindowTextW(hwnd, buf, buf.Capacity);
        return new ForegroundInfo(NormalizeAppName(image, uwp), buf.ToString());
    }

    static bool IsTransient(nint hwnd)
    {
        var cls = new StringBuilder(128);
        return GetClassNameW(hwnd, cls, cls.Capacity) > 0 && TransientClasses.Contains(cls.ToString());
    }

    static string? ImagePath(uint pid)
    {
        if (pid == 0) return null;
        var h = OpenProcess(0x1000 /* QUERY_LIMITED_INFORMATION */, false, pid);
        if (h == 0) return null;
        try
        {
            var sb = new StringBuilder(1024);
            int n = sb.Capacity;
            return QueryFullProcessImageNameW(h, 0, sb, ref n) ? sb.ToString() : null;
        }
        finally { CloseHandle(h); }
    }

    static string? HostedAppName(nint frame, uint hostPid)
    {
        uint found = 0;
        EnumChildWindows(frame, (child, _) =>
        {
            var cls = new StringBuilder(64);
            if (GetClassNameW(child, cls, cls.Capacity) > 0 && cls.ToString() == "Windows.UI.Core.CoreWindow")
            {
                GetWindowThreadProcessId(child, out uint p);
                if (p != 0 && p != hostPid) { found = p; return false; }
            }
            return true;
        }, 0);
        var path = found == 0 ? null : ImagePath(found);
        return path is null ? null : Path.GetFileNameWithoutExtension(path);
    }

    // ---- watcher: runs the message loop on the calling thread (STA) ----

    delegate void WinEventProc(nint hook, uint evt, nint hwnd, int idObject, int idChild, uint thread, uint time);
    delegate nint WndProc(nint hwnd, uint msg, nint wParam, nint lParam);
    delegate bool EnumProc(nint hwnd, nint lParam);

    // Held in statics so the GC cannot collect the native callbacks.
    static WinEventProc? _winEvent;
    static WndProc? _wndProc;

    /// <summary>
    /// Blocks forever pumping messages. onChange fires when the foreground app
    /// or its title changes; onLock/onUnlock on workstation lock/unlock.
    /// </summary>
    public static void Run(Action onForegroundEvent, Action onLock, Action onUnlock)
    {
        const uint EVENT_SYSTEM_FOREGROUND = 3, EVENT_OBJECT_NAMECHANGE = 0x800C;
        const uint WINEVENT_OUTOFCONTEXT = 0, WINEVENT_SKIPOWNPROCESS = 2;
        const uint WM_WTSSESSION_CHANGE = 0x02B1;

        _winEvent = (_, evt, hwnd, idObject, _, _, _) =>
        {
            // Name changes: only the foreground window's own title (idObject 0 = OBJID_WINDOW).
            if (evt == EVENT_OBJECT_NAMECHANGE && (idObject != 0 || hwnd != GetForegroundWindow())) return;
            onForegroundEvent();
        };
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, 0, _winEvent, 0, 0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        SetWinEventHook(EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE, 0, _winEvent, 0, 0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);

        _wndProc = (hwnd, msg, wParam, lParam) =>
        {
            if (msg == WM_WTSSESSION_CHANGE)
            {
                if (wParam == 7) onLock();       // WTS_SESSION_LOCK
                else if (wParam == 8) onUnlock(); // WTS_SESSION_UNLOCK
                return 0;
            }
            return DefWindowProcW(hwnd, msg, wParam, lParam);
        };
        var wc = new WNDCLASSEXW
        {
            cbSize = (uint)Marshal.SizeOf<WNDCLASSEXW>(),
            lpfnWndProc = Marshal.GetFunctionPointerForDelegate(_wndProc),
            lpszClassName = "VmContextHelperSession",
        };
        RegisterClassExW(ref wc);
        // Hidden top-level (not message-only) window: it is never shown.
        var win = CreateWindowExW(0, wc.lpszClassName!,"", 0, 0, 0, 0, 0, 0, 0, 0, 0);
        if (win == 0 || !WTSRegisterSessionNotification(win, 0 /* NOTIFY_FOR_THIS_SESSION */))
            throw new InvalidOperationException("session notification registration failed");

        while (GetMessageW(out var m, 0, 0, 0) > 0)
        {
            TranslateMessage(ref m);
            DispatchMessageW(ref m);
        }
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct WNDCLASSEXW
    {
        public uint cbSize, style;
        public nint lpfnWndProc;
        public int cbClsExtra, cbWndExtra;
        public nint hInstance, hIcon, hCursor, hbrBackground;
        public string? lpszMenuName, lpszClassName;
        public nint hIconSm;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct MSG { public nint hwnd; public uint message; public nint wParam, lParam; public uint time; public int x, y; }

    [DllImport("user32")] static extern nint GetForegroundWindow();
    [DllImport("user32")] static extern uint GetWindowThreadProcessId(nint hwnd, out uint pid);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(nint hwnd, StringBuilder s, int max);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern int GetClassNameW(nint hwnd, StringBuilder s, int max);
    [DllImport("user32")] static extern bool EnumChildWindows(nint parent, EnumProc proc, nint lParam);
    [DllImport("user32")] static extern nint SetWinEventHook(uint min, uint max, nint mod, WinEventProc proc, uint pid, uint tid, uint flags);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern ushort RegisterClassExW(ref WNDCLASSEXW wc);
    [DllImport("user32", CharSet = CharSet.Unicode)] static extern nint CreateWindowExW(uint ex, string cls, string name, uint style, int x, int y, int w, int h, nint parent, nint menu, nint inst, nint param);
    [DllImport("user32")] static extern nint DefWindowProcW(nint hwnd, uint msg, nint w, nint l);
    [DllImport("user32")] static extern int GetMessageW(out MSG m, nint hwnd, uint min, uint max);
    [DllImport("user32")] static extern bool TranslateMessage(ref MSG m);
    [DllImport("user32")] static extern nint DispatchMessageW(ref MSG m);
    [DllImport("wtsapi32")] static extern bool WTSRegisterSessionNotification(nint hwnd, uint flags);
    [DllImport("kernel32")] static extern nint OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32")] static extern bool CloseHandle(nint h);
    [DllImport("kernel32", CharSet = CharSet.Unicode)] static extern bool QueryFullProcessImageNameW(nint h, uint flags, StringBuilder s, ref int size);
}
