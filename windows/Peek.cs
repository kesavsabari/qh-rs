using System.Runtime.InteropServices;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.System;

namespace Qh;

/// Ctrl+Alt+Space from anywhere: a small floating composer (Spotlight-style). Enter sends to the
/// current project's current tab and goes away; the main window stays where it was. Esc closes.
/// The hotkey is registered on the main window, its WM_HOTKEY caught with a subclass like Tray's.
sealed class Peek : IDisposable
{
    readonly IntPtr main;
    readonly Func<Project?> project;
    readonly SubclassProc proc;
    Window? win;
    TextBox? box;
    IntPtr before;
    const int HotId = 1;

    public Peek(IntPtr main, Func<Project?> project)
    {
        this.main = main; this.project = project;
        proc = WndProc;
        SetWindowSubclass(main, proc, 78, 0);
        RegisterHotKey(main, HotId, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 0x20 /* VK_SPACE */);
    }

    IntPtr WndProc(IntPtr h, uint msg, IntPtr w, IntPtr l, UIntPtr id, UIntPtr data)
    {
        if (msg == 0x0312 && (int)w == HotId) { if (win?.AppWindow.IsVisible == true) Hide(false); else Show(); return IntPtr.Zero; }
        return DefSubclassProc(h, msg, w, l);
    }

    void Show()
    {
        before = GetForegroundWindow();
        if (win == null) Build();
        var p = project();
        box!.PlaceholderText = p == null ? "Open a folder in qh first" : $"Ask qh in {p.Name}  ·  Enter sends  ·  Esc closes";
        box.Text = "";
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(win);
        var scale = GetDpiForWindow(hwnd) / 96.0;
        int w = (int)(560 * scale), h = (int)(120 * scale);
        var area = DisplayArea.GetFromWindowId(Win32Interop.GetWindowIdFromWindow(before != IntPtr.Zero ? before : main), DisplayAreaFallback.Primary).WorkArea;
        win!.AppWindow.MoveAndResize(new(area.X + (area.Width - w) / 2, area.Y + area.Height / 4, w, h));
        win.AppWindow.Show();
        win.Activate();
        SetForegroundWindow(hwnd);
        box.Focus(FocusState.Programmatic);
    }

    void Build()
    {
        win = new Window { Title = "qh peek" };
        var aw = win.AppWindow;
        aw.IsShownInSwitchers = false;
        if (aw.Presenter is OverlappedPresenter op)
        {
            // the thin border keeps Windows 11's rounded corners; no caption at all
            op.SetBorderAndTitleBar(true, false);
            op.IsAlwaysOnTop = true; op.IsResizable = false; op.IsMaximizable = false; op.IsMinimizable = false;
        }
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(win);
        int round = 2;   // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33 /* DWMWA_WINDOW_CORNER_PREFERENCE */, ref round, sizeof(int));
        box = new TextBox
        {
            FontFamily = Theme.Sans, FontSize = 16, BorderThickness = new Thickness(0), Background = new SolidColorBrush(Colors.Transparent),
            VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.NoWrap,
        };
        foreach (var k in new[] { "TextControlBackgroundFocused", "TextControlBackgroundPointerOver", "TextControlBorderBrushFocused", "TextControlBorderBrushPointerOver", "TextControlBorderBrush" })
            box.Resources[k] = new SolidColorBrush(Colors.Transparent);
        box.PreviewKeyDown += (_, e) =>
        {
            if (e.Key == VirtualKey.Enter) { e.Handled = true; Send(); }
            else if (e.Key == VirtualKey.Escape) { e.Handled = true; Hide(true); }
        };
        var dot = new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 8, Height = 8, Fill = Theme.Accent, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 0, 12, 0) };
        var row = new Grid { Padding = new Thickness(20, 0, 16, 0) };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.Children.Add(dot);
        Grid.SetColumn(box, 1);
        row.Children.Add(box);
        win.Content = new Border { Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), Child = row };
    }

    void Send()
    {
        var text = box!.Text.Trim();
        if (text.Length == 0 || project() is not { } p) return;
        p.Prompt(text);
        Hide(true);
    }

    /// hide, and hand focus back to whatever had it (not qh's main window, unless that was it)
    void Hide(bool restore)
    {
        win?.AppWindow.Hide();
        if (restore && before != IntPtr.Zero) SetForegroundWindow(before);
    }

    public void Dispose()
    {
        UnregisterHotKey(main, HotId);
        RemoveWindowSubclass(main, proc, 78);
        win?.Close();
    }

    const uint MOD_ALT = 1, MOD_CONTROL = 2, MOD_NOREPEAT = 0x4000;
    delegate IntPtr SubclassProc(IntPtr h, uint msg, IntPtr w, IntPtr l, UIntPtr id, UIntPtr data);

    [DllImport("user32.dll")] static extern bool RegisterHotKey(IntPtr h, int id, uint mods, uint vk);
    [DllImport("user32.dll")] static extern bool UnregisterHotKey(IntPtr h, int id);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("dwmapi.dll")] static extern int DwmSetWindowAttribute(IntPtr h, int attr, ref int value, int size);
    [DllImport("comctl32.dll")] static extern bool SetWindowSubclass(IntPtr h, SubclassProc p, uint id, uint data);
    [DllImport("comctl32.dll")] static extern bool RemoveWindowSubclass(IntPtr h, SubclassProc p, uint id);
    [DllImport("comctl32.dll")] static extern IntPtr DefSubclassProc(IntPtr h, uint msg, IntPtr w, IntPtr l);
}
