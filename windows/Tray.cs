using System.Runtime.InteropServices;

namespace Qh;

/// The tray icon (the Mac's menu bar extra): one small ring per running agent, orange when one is
/// waiting for you. Click = bring qh up; right-click = Open / Quit. Plain Shell_NotifyIcon, drawn
/// pixel by pixel, so it costs no extra library.
sealed class Tray : IDisposable
{
    readonly IntPtr hwnd;
    readonly Action open, quit;
    IntPtr icon;
    string lastKey = "";
    const int Msg = 0x8000 + 77;   // WM_APP + 77
    const uint Id = 1;
    readonly SubclassProc proc;

    public Tray(IntPtr hwnd, Action open, Action quit)
    {
        this.hwnd = hwnd; this.open = open; this.quit = quit;
        proc = WndProc;
        SetWindowSubclass(hwnd, proc, 77, 0);
        var d = Data(NIF_MESSAGE | NIF_ICON | NIF_TIP, "qh");
        d.hIcon = icon = Draw(0, 0);
        Shell_NotifyIcon(NIM_ADD, ref d);
    }

    /// running = agents working, waiting = asks open
    public void Update(int running, int waiting)
    {
        var key = $"{running}/{waiting}";
        if (key == lastKey) return;
        lastKey = key;
        var old = icon;
        var tip = running == 0 && waiting == 0 ? "qh" : $"qh · {running} working" + (waiting > 0 ? $" · {waiting} waiting for you" : "");
        var d = Data(NIF_ICON | NIF_TIP, tip);
        d.hIcon = icon = Draw(running, waiting);
        Shell_NotifyIcon(NIM_MODIFY, ref d);
        if (old != IntPtr.Zero) DestroyIcon(old);
    }

    /// 32×32: up to three rings in a row; ink while working, orange if any agent waits; a dot when idle.
    static IntPtr Draw(int running, int waiting)
    {
        const int S = 32;
        var px = new uint[S * S];
        var dark = IsDarkTaskbar();
        uint ink = dark ? 0xFFECEBE6 : 0xFF0E0E0D, orange = 0xFFFF4D1C;
        int n = Math.Min(3, Math.Max(running, waiting > 0 ? 1 : 0));
        void Ring(double cx, double cy, double r, double w, uint c, bool fill)
        {
            for (int y = 0; y < S; y++) for (int x = 0; x < S; x++)
            {
                var d = Math.Sqrt((x + 0.5 - cx) * (x + 0.5 - cx) + (y + 0.5 - cy) * (y + 0.5 - cy));
                var a = fill ? Math.Clamp(r - d + 0.5, 0, 1) : Math.Clamp(w / 2 - Math.Abs(d - r) + 0.5, 0, 1);
                if (a > 0) px[y * S + x] = ((uint)(a * 255) << 24) | (c & 0xFFFFFF);
            }
        }
        if (n == 0) Ring(16, 16, 4.5, 0, ink, true);
        else
        {
            double r = n == 1 ? 9 : n == 2 ? 6.5 : 4.6, gap = 2 * r + 2.5, x0 = 16 - (n - 1) * gap / 2;
            for (int i = 0; i < n; i++) Ring(x0 + i * gap, 16, r, 2.6, i == 0 && waiting > 0 ? orange : ink, false);
        }
        // premultiply for the icon's alpha
        for (int i = 0; i < px.Length; i++)
        {
            uint a = px[i] >> 24;
            px[i] = (a << 24) | ((((px[i] >> 16) & 0xFF) * a / 255) << 16) | ((((px[i] >> 8) & 0xFF) * a / 255) << 8) | ((px[i] & 0xFF) * a / 255);
        }
        var color = CreateBitmap(S, S, 1, 32, px);
        var mask = CreateBitmap(S, S, 1, 1, new byte[S * S / 8]);
        var info = new ICONINFO { fIcon = true, hbmColor = color, hbmMask = mask };
        var h = CreateIconIndirect(ref info);
        DeleteObject(color); DeleteObject(mask);
        return h;
    }

    static bool IsDarkTaskbar()
    {
        try
        {
            using var k = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
            return k?.GetValue("SystemUsesLightTheme") is int v && v == 0;
        }
        catch { return true; }
    }

    IntPtr WndProc(IntPtr h, uint msg, IntPtr w, IntPtr l, UIntPtr id, UIntPtr data)
    {
        if (msg == Msg)
        {
            var ev = (uint)l & 0xFFFF;
            if (ev == 0x0202) open();                 // WM_LBUTTONUP
            else if (ev == 0x0205) Menu();            // WM_RBUTTONUP
            return IntPtr.Zero;
        }
        return DefSubclassProc(h, msg, w, l);
    }

    void Menu()
    {
        var m = CreatePopupMenu();
        AppendMenu(m, 0, 1, "Open qh");
        AppendMenu(m, 0x800, 0, null);                // separator
        AppendMenu(m, 0, 2, "Quit qh");
        GetCursorPos(out var p);
        SetForegroundWindow(hwnd);
        var pick = TrackPopupMenu(m, 0x100 /* TPM_RETURNCMD */, p.X, p.Y, 0, hwnd, IntPtr.Zero);
        DestroyMenu(m);
        if (pick == 1) open(); else if (pick == 2) quit();
    }

    NOTIFYICONDATA Data(uint flags, string tip) => new()
    {
        cbSize = (uint)Marshal.SizeOf<NOTIFYICONDATA>(), hWnd = hwnd, uID = Id, uFlags = flags, uCallbackMessage = Msg, szTip = tip,
    };

    public void Dispose()
    {
        var d = Data(0, "");
        Shell_NotifyIcon(NIM_DELETE, ref d);
        if (icon != IntPtr.Zero) DestroyIcon(icon);
        RemoveWindowSubclass(hwnd, proc, 77);
    }

    const uint NIM_ADD = 0, NIM_MODIFY = 1, NIM_DELETE = 2, NIF_MESSAGE = 1, NIF_ICON = 2, NIF_TIP = 4;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct NOTIFYICONDATA
    {
        public uint cbSize; public IntPtr hWnd; public uint uID, uFlags, uCallbackMessage; public IntPtr hIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string szTip;
        public uint dwState, dwStateMask;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string szInfo;
        public uint uVersion;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string szInfoTitle;
        public uint dwInfoFlags; public Guid guidItem; public IntPtr hBalloonIcon;
    }
    [StructLayout(LayoutKind.Sequential)] struct ICONINFO { public bool fIcon; public int x, y; public IntPtr hbmMask, hbmColor; }
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    delegate IntPtr SubclassProc(IntPtr h, uint msg, IntPtr w, IntPtr l, UIntPtr id, UIntPtr data);

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)] static extern bool Shell_NotifyIcon(uint msg, ref NOTIFYICONDATA d);
    [DllImport("comctl32.dll")] static extern bool SetWindowSubclass(IntPtr h, SubclassProc p, uint id, uint data);
    [DllImport("comctl32.dll")] static extern bool RemoveWindowSubclass(IntPtr h, SubclassProc p, uint id);
    [DllImport("comctl32.dll")] static extern IntPtr DefSubclassProc(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("gdi32.dll")] static extern IntPtr CreateBitmap(int w, int h, uint planes, uint bpp, uint[] bits);
    [DllImport("gdi32.dll")] static extern IntPtr CreateBitmap(int w, int h, uint planes, uint bpp, byte[] bits);
    [DllImport("gdi32.dll")] static extern bool DeleteObject(IntPtr o);
    [DllImport("user32.dll")] static extern IntPtr CreateIconIndirect(ref ICONINFO i);
    [DllImport("user32.dll")] static extern bool DestroyIcon(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr CreatePopupMenu();
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool AppendMenu(IntPtr m, uint flags, uint id, string? text);
    [DllImport("user32.dll")] static extern int TrackPopupMenu(IntPtr m, uint flags, int x, int y, int r, IntPtr h, IntPtr rect);
    [DllImport("user32.dll")] static extern bool DestroyMenu(IntPtr m);
    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
}
