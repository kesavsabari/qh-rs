using System.Runtime.InteropServices;
using Microsoft.UI;
using Microsoft.UI.Input;
using Microsoft.UI.Text;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.System;
using Windows.UI;

namespace Qh;

/// The window, the same Zen layout as apple/qh/ContentView.swift: a sidebar on the warm ground,
/// the work floating on it as a card. No visible title bar: like Zen browser the caption buttons
/// stay hidden until the pointer reaches the top-right corner; Snap Layouts still work there.
sealed class MainWindow : Window
{
    readonly Workspace ws;
    readonly Grid root = new();
    readonly StackPanel sidebar = new();
    readonly Grid card = new();
    readonly StackPanel tabBar = new() { Orientation = Orientation.Horizontal, Spacing = 2, VerticalAlignment = VerticalAlignment.Bottom };
    readonly ContentControl thread = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch, VerticalContentAlignment = VerticalAlignment.Stretch };
    readonly TextBox input = new();
    readonly TextBlock meta = new();
    readonly Border send = new();
    bool captionsShown = true;

    public MainWindow()
    {
        Title = "qh";
        ws = new Workspace(DispatcherQueue);
        ws.Changed += Render;
        Closed += (_, _) => ws.Shutdown();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1280, 820));
        Chrome();
        Build();
        Render();
    }

    // MARK: window chrome

    void Chrome()
    {
        ExtendsContentIntoTitleBar = true;
        AppWindow.TitleBar.PreferredHeightOption = TitleBarHeightOption.Tall;
        SetCaptions(false);
        // poll the pointer: over the caption buttons the app gets no pointer events (that's non-client)
        var t = DispatcherQueue.CreateTimer();
        t.Interval = TimeSpan.FromMilliseconds(120);
        t.Tick += (_, _) => SetCaptions(NearCorner());
        t.Start();
    }

    bool NearCorner()
    {
        if (!GetCursorPos(out var p)) return false;
        var pos = AppWindow.Position; var size = AppWindow.Size;
        int x = p.X - pos.X, y = p.Y - pos.Y;
        var scale = (Content?.XamlRoot?.RasterizationScale) ?? 1.0;
        return y >= 0 && y < 56 * scale && x <= size.Width && x > size.Width - 200 * scale;
    }

    void SetCaptions(bool show)
    {
        if (show == captionsShown) return;
        captionsShown = show;
        var tb = AppWindow.TitleBar;
        Color clear = Colors.Transparent, fg = show ? Theme.Soft.Color : clear;
        tb.ButtonBackgroundColor = clear;
        tb.ButtonInactiveBackgroundColor = clear;
        tb.ButtonForegroundColor = fg;
        tb.ButtonInactiveForegroundColor = fg;
        tb.ButtonHoverBackgroundColor = show ? Theme.Hover.Color : clear;
        tb.ButtonHoverForegroundColor = show ? Theme.Ink.Color : clear;
        tb.ButtonPressedBackgroundColor = show ? Theme.Line.Color : clear;
    }

    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);

    // MARK: layout

    void Build()
    {
        root.Background = Theme.Ground;
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(44) });
        root.RowDefinitions.Add(new RowDefinition());
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(260) });
        root.ColumnDefinitions.Add(new ColumnDefinition());

        // the whole top strip of ground drags the window
        var drag = new Border { Background = Theme.Ground };
        Grid.SetColumnSpan(drag, 2);
        root.Children.Add(drag);
        SetTitleBar(drag);

        var side = new Grid();
        side.RowDefinitions.Add(new RowDefinition());
        side.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        side.Children.Add(new ScrollViewer { Content = sidebar });
        var bottom = SidebarBottom();
        Grid.SetRow(bottom, 1);
        side.Children.Add(bottom);
        Grid.SetRow(side, 1);
        root.Children.Add(side);

        var frame = new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(0.5),
            CornerRadius = new CornerRadius(12), Margin = new Thickness(0, 0, 8, 8), Child = card,
        };
        Grid.SetRow(frame, 1);
        Grid.SetColumn(frame, 1);
        root.Children.Add(frame);

        card.RowDefinitions.Add(new RowDefinition { Height = new GridLength(44) });
        card.RowDefinitions.Add(new RowDefinition());
        card.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        tabBar.Margin = new Thickness(12, 0, 12, 0);
        card.Children.Add(tabBar);
        Grid.SetRow(thread, 1);
        card.Children.Add(thread);
        var composer = Composer();
        Grid.SetRow(composer, 2);
        card.Children.Add(composer);

        Key(VirtualKey.T, () => ws.Current?.NewTab());
        Key(VirtualKey.O, Pick);
        Content = root;
    }

    void Key(VirtualKey k, Action a)
    {
        var acc = new KeyboardAccelerator { Key = k, Modifiers = VirtualKeyModifiers.Control };
        acc.Invoked += (_, e) => { e.Handled = true; a(); };
        root.KeyboardAccelerators.Add(acc);
    }

    FrameworkElement SidebarBottom()
    {
        var row = new Grid { Padding = new Thickness(14, 10, 14, 10) };
        // personal ⇄ team as Zen's workspace dots; team comes with W4
        var dots = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, VerticalAlignment = VerticalAlignment.Center };
        dots.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 8, Height = 8, Fill = Theme.Ink });
        dots.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 6, Height = 6, Fill = Theme.Faint });
        row.Children.Add(dots);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        right.Children.Add(Plain(Icon(""), Pick, "Open folder (Ctrl+O)"));
        right.Children.Add(Plain(Icon(""), () => { }, "Settings"));
        row.Children.Add(right);
        return row;
    }

    // MARK: render (state → views; the composer stays, so typing survives updates)

    void Render()
    {
        RenderSidebar();
        RenderTabs();
        RenderThread();
        var t = ws.Current?.Current;
        meta.Text = t == null ? "" : string.Join("   ", new[] { Theme.ShortModel(t.Model), ws.Current!.Mode }.Where(s => s.Length > 0));
        send.Background = t?.Busy == true ? Theme.Accent : input.Text.Length > 0 ? Theme.Ink : Theme.Faint;
    }

    void RenderSidebar()
    {
        sidebar.Children.Clear();
        if (ws.Recent.Count == 0)
        {
            sidebar.Children.Add(Label("Open a folder to start.  Ctrl+O", 13, Theme.Soft, new Thickness(16, 8, 16, 0)));
            return;
        }
        // projects as tiles, three to a row (Zen's Essentials)
        var grid = new Grid { Margin = new Thickness(12, 4, 12, 0), ColumnSpacing = 6, RowSpacing = 6 };
        for (int c = 0; c < 3; c++) grid.ColumnDefinitions.Add(new ColumnDefinition());
        for (int i = 0; i < ws.Recent.Count; i++)
        {
            if (i % 3 == 0) grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(44) });
            var tile = Tile(ws.Recent[i]);
            Grid.SetRow(tile, i / 3);
            Grid.SetColumn(tile, i % 3);
            grid.Children.Add(tile);
        }
        sidebar.Children.Add(grid);
        if (ws.Current is not { } cur) return;

        var nw = new Grid { Padding = new Thickness(8, 6, 8, 6) };
        nw.Children.Add(new StackPanel
        {
            Orientation = Orientation.Horizontal, Spacing = 10,
            Children = { Icon("", 11), Label("New chat", 13, Theme.Soft) },
        });
        nw.Children.Add(new TextBlock { Text = "Ctrl+T", FontFamily = Theme.Mono, FontSize = 11, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center });
        sidebar.Children.Add(Row(nw, false, cur.NewTab, new Thickness(8, 12, 8, 0)));
        sidebar.Children.Add(Label(cur.Name.ToUpperInvariant(), 11, Theme.Faint, new Thickness(16, 12, 16, 4)));
        foreach (var t in cur.Tabs)
        {
            var name = t.Name;
            sidebar.Children.Add(Row(Label(t.Title, 13, Theme.Ink), t == cur.Current, () => { cur.CurrentTab = name; Render(); }, new Thickness(8, 0, 8, 0)));
        }
    }

    FrameworkElement Tile(string path)
    {
        bool on = ws.Current?.Path == path;
        var b = new Border
        {
            CornerRadius = new CornerRadius(10), Background = on ? Theme.Card : Theme.Tile,
            Child = new TextBlock
            {
                Text = Path.GetFileName(path.TrimEnd('\\')), FontFamily = on ? Theme.SansMedium : Theme.Sans, FontSize = 12,
                Foreground = on ? Theme.Ink : Theme.Soft, TextTrimming = TextTrimming.CharacterEllipsis,
                HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(6, 0, 6, 0),
            },
        };
        if (on) b.Shadow = new ThemeShadow();
        Hover(b, on ? Theme.Card : Theme.Tile);
        b.Tapped += (_, _) => ws.Open(path);
        ToolTipService.SetToolTip(b, path);
        var menu = new MenuFlyout();
        var show = new MenuFlyoutItem { Text = "Show in Explorer" };
        show.Click += (_, _) => System.Diagnostics.Process.Start("explorer.exe", $"\"{path}\"");
        var forget = new MenuFlyoutItem { Text = "Remove from list" };
        forget.Click += (_, _) => ws.Forget(path);
        menu.Items.Add(show);
        menu.Items.Add(forget);
        b.ContextFlyout = menu;
        return b;
    }

    void RenderTabs()
    {
        tabBar.Children.Clear();
        if (ws.Current is not { } p) return;
        foreach (var t in p.Tabs)
        {
            bool on = t == p.Current;
            var name = t.Name;
            var dot = new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 5, Height = 5, Fill = t.Busy ? Theme.Accent : Theme.Faint, VerticalAlignment = VerticalAlignment.Center };
            var inner = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 7, Children = { dot, Label(t.Title, 12.5, on ? Theme.Ink : Theme.Soft) } };
            var b = new Border { Padding = new Thickness(10, 5, 10, 5), CornerRadius = new CornerRadius(7), Background = on ? Theme.Tile : null, Child = inner, Margin = new Thickness(0, 0, 0, 6) };
            b.Tapped += (_, _) => { p.CurrentTab = name; Render(); };
            tabBar.Children.Add(b);
        }
        // + goes after the tabs
        var plus = Plain(Label("+", 15, Theme.Faint), p.NewTab, "New tab (Ctrl+T)");
        plus.Margin = new Thickness(4, 0, 0, 10);
        tabBar.Children.Add(plus);
    }

    void RenderThread()
    {
        var p = ws.Current;
        string? hint = ws.Qh == null ? "qh isn't installed. Install it with install.ps1, then reopen."
            : p == null ? "Open a folder to start."
            : p.State.StartsWith("closed") ? p.State[8..]
            : p.State == "starting" ? $"starting qh in {p.Name}…"
            : p.NeedsSetup ? "Connect a model: run qh once in a terminal for now (setup comes to the app in W2)."
            : p.Current is not { } t || t.Messages.Count == 0 ? "Ask qh anything about this project." : null;
        if (hint != null)
        {
            thread.Content = new TextBlock
            {
                Text = hint, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap,
                HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(32, 56, 32, 0),
            };
            return;
        }
        // W1: plain text turns; markdown, tool cards and approvals are W2
        var col = new StackPanel { Spacing = 18, MaxWidth = 760, Padding = new Thickness(32, 24, 32, 24) };
        foreach (var (role, text) in p!.Current!.Messages.Where(m => m.Text.Length > 0))
        {
            var tb = new TextBlock { Text = text, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Ink, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true, LineHeight = 22 };
            col.Children.Add(role == "user"
                ? new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(12), Padding = new Thickness(14, 10, 14, 10), HorizontalAlignment = HorizontalAlignment.Right, MaxWidth = 560, Child = tb }
                : tb);
        }
        var sv = new ScrollViewer { Content = col };
        sv.Loaded += (_, _) => sv.ChangeView(null, sv.ScrollableHeight, null, true);
        thread.Content = sv;
    }

    FrameworkElement Composer()
    {
        input.PlaceholderText = "Ask anything · / commands · @ files";
        input.AcceptsReturn = true;
        input.TextWrapping = TextWrapping.Wrap;
        input.MaxHeight = 220;
        input.FontFamily = Theme.Sans;
        input.FontSize = 14;
        input.BorderThickness = new Thickness(0);
        input.Background = new SolidColorBrush(Colors.Transparent);
        // no Fluent focus underline or fill: the card around it is the field
        foreach (var k in new[] { "TextControlBackgroundFocused", "TextControlBackgroundPointerOver" })
            input.Resources[k] = new SolidColorBrush(Colors.Transparent);
        foreach (var k in new[] { "TextControlBorderBrushFocused", "TextControlBorderBrushPointerOver", "TextControlBorderBrush" })
            input.Resources[k] = new SolidColorBrush(Colors.Transparent);
        input.TextChanged += (_, _) => send.Background = input.Text.Length > 0 ? Theme.Ink : Theme.Faint;
        // ⏎ sends, Shift+⏎ is a newline, Esc stops
        input.PreviewKeyDown += (_, e) =>
        {
            var shift = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
            if (e.Key == VirtualKey.Enter && !shift) { e.Handled = true; Send(); }
            else if (e.Key == VirtualKey.Escape) { e.Handled = true; ws.Current?.Stop(); }
        };

        meta.FontFamily = Theme.Sans;
        meta.FontSize = 11.5;
        meta.Foreground = Theme.Faint;
        meta.VerticalAlignment = VerticalAlignment.Center;

        send.Width = 28; send.Height = 28;
        send.CornerRadius = new CornerRadius(8);
        send.Background = Theme.Faint;
        send.Child = new FontIcon { Glyph = "", FontSize = 12, Foreground = Theme.Card };
        send.Tapped += (_, _) => { if (ws.Current?.Current?.Busy == true) ws.Current.Stop(); else Send(); };

        var bar = new Grid { Margin = new Thickness(8, 2, 8, 8) };
        var left = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 14 };
        left.Children.Add(Plain(Icon("", 12), () => { }, "Attach an image"));
        left.Children.Add(meta);
        bar.Children.Add(left);
        send.HorizontalAlignment = HorizontalAlignment.Right;
        bar.Children.Add(send);

        var box = new StackPanel { Children = { input, bar } };
        return new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(12),
            Margin = new Thickness(0, 0, 0, 16), MaxWidth = 760, HorizontalAlignment = HorizontalAlignment.Stretch, Padding = new Thickness(4, 4, 4, 0),
            Child = box,
        };
    }

    void Send()
    {
        if (ws.Current is not { } p || input.Text.Trim().Length == 0) return;
        p.Prompt(input.Text);
        input.Text = "";
    }

    async void Pick()
    {
        var picker = new Windows.Storage.Pickers.FolderPicker();
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        if (await picker.PickSingleFolderAsync() is { } f) ws.Open(f.Path);
    }

    // MARK: small pieces

    static TextBlock Label(string text, double size, Brush fg, Thickness? margin = null) => new()
    {
        Text = text, FontFamily = Theme.Sans, FontSize = size, Foreground = fg, TextTrimming = TextTrimming.CharacterEllipsis,
        VerticalAlignment = VerticalAlignment.Center, Margin = margin ?? new Thickness(0),
    };

    static FontIcon Icon(string glyph, double size = 13) => new() { Glyph = glyph, FontSize = size, Foreground = Theme.Soft };

    /// A borderless control: content only, a soft fill on hover (SwiftUI's .buttonStyle(.hand)).
    static Border Plain(UIElement content, Action action, string tip)
    {
        var b = new Border { Child = content, Padding = new Thickness(4), CornerRadius = new CornerRadius(6), Background = new SolidColorBrush(Colors.Transparent) };
        Hover(b, new SolidColorBrush(Colors.Transparent));
        b.Tapped += (_, _) => action();
        ToolTipService.SetToolTip(b, tip);
        return b;
    }

    /// A sidebar row: full width, selected = tile fill.
    static Border Row(UIElement content, bool selected, Action action, Thickness margin)
    {
        var rest = selected ? Theme.Tile : new SolidColorBrush(Colors.Transparent);
        var b = new Border { Child = content, Padding = new Thickness(8, 6, 8, 6), CornerRadius = new CornerRadius(7), Background = rest, Margin = margin };
        Hover(b, rest);
        b.Tapped += (_, _) => action();
        return b;
    }

    static void Hover(Border b, Brush rest)
    {
        b.PointerEntered += (_, _) => b.Background = Theme.Hover;
        b.PointerExited += (_, _) => b.Background = rest;
    }
}
