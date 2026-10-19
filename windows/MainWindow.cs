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
    readonly ThreadView threadView = new();
    readonly StackPanel asks = new() { Padding = new Thickness(16, 0, 16, 0) };
    readonly StackPanel suggestions = new() { Spacing = 1 };
    readonly Border suggestBox = new();
    readonly StackPanel tabRight = new() { Orientation = Orientation.Horizontal, Spacing = 14, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 0, 16, 0) };
    List<(string Insert, string Label, string About)> sugg = [];
    string? reviewPick;
    int suggSel;
    readonly TextBox input = new();
    readonly TextBlock meta = new();
    readonly Border send = new();
    bool captionsShown = true;

    public MainWindow()
    {
        Title = "qh";
        ws = new Workspace(DispatcherQueue);
        ws.Changed += Render;
        ws.ItemChanged += (t, i, added) => { threadView.Changed(t, i, added); if (added) RenderMeta(); };
        Closed += (_, _) => ws.Shutdown();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1280, 820));
        // CI screenshots: fill the screen so the corner hover lands on the window
        if (Environment.GetEnvironmentVariable("QH_APP_MAXIMIZE") == "1" && AppWindow.Presenter is OverlappedPresenter op) op.Maximize();
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
        // the ground colour, not transparent: Windows still draws the glyphs at alpha 0
        Color clear = Theme.Ground.Color, fg = show ? Theme.Soft.Color : clear;
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
        card.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        tabBar.Margin = new Thickness(12, 0, 12, 0);
        card.Children.Add(tabBar);
        card.Children.Add(tabRight);
        Grid.SetRow(thread, 1);
        card.Children.Add(thread);
        Grid.SetRow(asks, 2);
        card.Children.Add(asks);
        var composer = Composer();
        Grid.SetRow(composer, 3);
        card.Children.Add(composer);

        Key(VirtualKey.T, () => ws.Current?.NewTab());
        Key(VirtualKey.Number1, () => { if (ws.Current is { } p) SetLayout(p, "chat"); });
        Key(VirtualKey.Number2, () => { if (ws.Current is { } p) SetLayout(p, "split"); });
        Key(VirtualKey.Number3, () => { if (ws.Current is { } p) SetLayout(p, "code"); });
        Key(VirtualKey.D, () => { if (ws.Current is { } p) Page(p, "review"); });
        // paths in replies open in the code view
        Markdown.IsFile = s => ws.Current?.Files.Contains(s.Split(':')[0].Replace('\\', '/')) == true;
        Markdown.Open = s => ws.Current?.OpenFile(s);
        Key(VirtualKey.O, Pick);
        // the first waiting permission: Ctrl+Enter allows once, Ctrl+Backspace denies
        Key(VirtualKey.Enter, () => { if (FirstAsk() is { } a) ws.Current!.Answer(a, "yes"); });
        Key(VirtualKey.Back, () => { if (FirstAsk() is { } a) ws.Current!.Answer(a, "no"); });
        Content = root;
    }

    Ask? FirstAsk() => ws.Current is { } p && p.Current is { } t ? p.Asks.FirstOrDefault(a => a.Tab == t.Name) : null;

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
        RenderAsks();
        RenderMeta();
    }

    void RenderMeta()
    {
        var t = ws.Current?.Current;
        var bits = t == null ? [] : new List<string> { Theme.ShortModel(t.Model, t.Provider), ws.Current!.Mode };
        if (t is { Tokens: >= 1000 }) bits.Add($"{t.Tokens / 1000}k tokens");
        if (t?.Cost is > 0) bits.Add($"${t.Cost:0.000}");
        meta.Text = string.Join("   ", bits.Where(s => s.Length > 0));
        var busy = t?.Busy == true;
        send.Background = busy ? Theme.Accent : input.Text.Length > 0 ? Theme.Ink : Theme.Faint;
        ((FontIcon)send.Child).Glyph = busy ? "\uE71A" : "\uE74A";
    }

    void RenderAsks()
    {
        asks.Children.Clear();
        if (ws.Current is not { } p || p.Current is not { } t) return;
        foreach (var a in p.Asks.Where(a => a.Tab == t.Name)) asks.Children.Add(Cards.Ask(a, p));
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
        tabRight.Children.Clear();
        if (ws.Current is not { } p) return;
        if (p.Changes.Count > 0)
            tabRight.Children.Add(Plain(Label($"{p.Changes.Count} changed", 12.5, Theme.Accent), () => Page(p, "review"), "Review changes (Ctrl+D)"));
        tabRight.Children.Add(Plain(Label("history", 12.5, p.Page == "history" ? Theme.Ink : Theme.Soft), () => Page(p, "history"), "Checkpoints"));
        tabRight.Children.Add(LayoutSwitch(p));
        var models = Plain(Label("models", 12.5, p.ShowSetup ? Theme.Ink : Theme.Soft), () => { p.ShowSetup = !p.ShowSetup; if (p.ShowSetup) p.LoadSetup(); Render(); }, "Connect models");
        tabRight.Children.Add(models);
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

    void Page(Project p, string page)
    {
        p.Page = p.Page == page ? null : page;
        if (p.Page == "review") p.LoadChanges();
        if (p.Page == "history") p.LoadCheckpoints();
        Render();
    }

    /// Chat | Split | Code, as a quiet segmented control (Ctrl+1–3).
    FrameworkElement LayoutSwitch(Project p)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, Padding = new Thickness(2) };
        foreach (var (label, tag) in new[] { ("Chat", "chat"), ("Split", "split"), ("Code", "code") })
        {
            var on = p.Layout == tag && p.Page == null;
            var b = new Border { CornerRadius = new CornerRadius(6), Padding = new Thickness(10, 3, 10, 3), Background = on ? Theme.Card : null, Child = Label(label, 12, on ? Theme.Ink : Theme.Soft) };
            b.Tapped += (_, _) => SetLayout(p, tag);
            row.Children.Add(b);
        }
        return new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(8), Child = row };
    }

    void SetLayout(Project p, string tag) { p.Layout = tag; p.Page = null; p.ShowSetup = false; Render(); }

    /// the thread's one view can only sit in one place: lift it out before reusing it
    void Detach()
    {
        thread.Content = null;
        if (threadView.View.Parent is Panel pp) pp.Children.Remove(threadView.View);
    }

    static Grid Columns(UIElement left, UIElement right, double leftWidth)
    {
        var g = new Grid();
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = leftWidth > 0 ? new GridLength(leftWidth) : new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(left);
        var line = new Border { BorderBrush = Theme.Line, BorderThickness = new Thickness(1, 0, 0, 0), Child = right };
        Grid.SetColumn(line, 1);
        g.Children.Add(line);
        return g;
    }

    void RenderThread()
    {
        var p = ws.Current;
        Detach();
        if (p is { ShowSetup: true, State: "ready" }) { thread.Content = Cards.Setup(p); return; }
        if (p is { State: "ready" })
        {
            if (p.Draft.Length > 0) { input.Text = p.Draft; input.SelectionStart = input.Text.Length; p.Draft = ""; }
            if (p.Page == "review") { thread.Content = CodeViews.Review(p, reviewPick, path => { reviewPick = path; Render(); }); return; }
            if (p.Page == "history") { thread.Content = CodeViews.History(p); return; }
            if (p.Layout == "code") { thread.Content = Columns(CodeViews.Tree(p), CodeViews.Code(p), 260); return; }
        }
        string? hint = ws.Qh == null ? "qh isn't installed. Install it with install.ps1, then reopen."
            : p == null ? "Open a folder to start."
            : p.State.StartsWith("closed") ? p.State[8..]
            : p.State == "starting" ? $"starting qh in {p.Name}…"
            : p.Current is not { } t || t.Items.Count == 0 ? "Ask qh anything about this project." : null;
        if (hint != null)
        {
            thread.Content = new TextBlock
            {
                Text = hint, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap,
                HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(32, 56, 32, 0),
            };
            return;
        }
        threadView.Show(p!.Current!);
        // Split shows the code beside the chat once a file is open; until then, just the chat
        thread.Content = p.Layout == "split" && p.Code != null ? Columns(threadView.View, CodeViews.Code(p), 0) : threadView.View;
    }

    // MARK: composer suggestions: / commands and @ files, like the Mac's popup

    void Suggest()
    {
        sugg = [];
        var p = ws.Current;
        var text = input.Text;
        if (p != null && text.StartsWith('/') && !text.Contains(' '))
            sugg = p.Commands.Where(c => c.Cmd.StartsWith(text, StringComparison.OrdinalIgnoreCase)).Take(8).Select(c => (c.Cmd + " ", c.Cmd, c.About)).ToList();
        else if (p != null && LastWord(text) is { } w && w.StartsWith('@'))
        {
            var q = w[1..].ToLowerInvariant();
            sugg = p.Files.Select(f => (f, score: Fuzzy(f.ToLowerInvariant(), q))).Where(x => x.score >= 0)
                .OrderBy(x => x.score).ThenBy(x => x.f.Length).Take(8).Select(x => ("@" + x.f + " ", x.f, "")).ToList();
        }
        suggSel = 0;
        DrawSuggestions();
    }

    static string? LastWord(string s) { var i = s.LastIndexOfAny([' ', '\n']); return s[(i + 1)..]; }

    /// lower is better; -1 = not a match. Letters in order, a hit in the file name beats one in the folders.
    static int Fuzzy(string f, string q)
    {
        if (q.Length == 0) return f.Count(c => c == '/');
        int at = 0, gaps = 0;
        foreach (var c in q) { var k = f.IndexOf(c, at); if (k < 0) return -1; gaps += k - at; at = k + 1; }
        return gaps + (f[(f.LastIndexOf('/') + 1)..].Contains(q) ? 0 : 50);
    }

    void DrawSuggestions()
    {
        suggestions.Children.Clear();
        suggestBox.Visibility = sugg.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        for (int i = 0; i < sugg.Count; i++)
        {
            var (_, label, about) = sugg[i];
            var g = new Grid { Padding = new Thickness(10, 5, 10, 5) };
            g.Children.Add(new TextBlock { Text = label, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Ink, TextTrimming = TextTrimming.CharacterEllipsis });
            g.Children.Add(new TextBlock { Text = about, FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = 360 });
            var k = i;
            var b = new Border { Child = g, CornerRadius = new CornerRadius(6), Background = i == suggSel ? Theme.Tile : new SolidColorBrush(Colors.Transparent) };
            b.Tapped += (_, _) => { suggSel = k; Accept(); };
            suggestions.Children.Add(b);
        }
    }

    void Accept()
    {
        if (suggSel >= sugg.Count) return;
        var ins = sugg[suggSel].Insert;
        var t = input.Text;
        input.Text = ins.StartsWith('@') ? t[..(t.Length - (LastWord(t)?.Length ?? 0))] + ins : ins;
        input.SelectionStart = input.Text.Length;
        sugg = [];
        DrawSuggestions();
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
        input.TextChanged += (_, _) => { RenderMeta(); Suggest(); };
        // ⏎ sends, Shift+⏎ is a newline, Esc stops
        input.PreviewKeyDown += (_, e) =>
        {
            var shift = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
            if (sugg.Count > 0 && e.Key is VirtualKey.Down or VirtualKey.Up)
            { e.Handled = true; suggSel = (suggSel + (e.Key == VirtualKey.Down ? 1 : sugg.Count - 1)) % sugg.Count; DrawSuggestions(); }
            else if (sugg.Count > 0 && (e.Key == VirtualKey.Tab || (e.Key == VirtualKey.Enter && !shift)))
            { e.Handled = true; Accept(); }
            else if (e.Key == VirtualKey.Enter && !shift) { e.Handled = true; Send(); }
            else if (e.Key == VirtualKey.Escape && ws.Current is { Page: not null } pg) { e.Handled = true; pg.Page = null; Render(); }
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
        ToolTipService.SetToolTip(send, "Send (Enter) · Stop (Esc)");

        var bar = new Grid { Margin = new Thickness(8, 2, 8, 8) };
        var left = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 14 };
        left.Children.Add(Plain(Icon("", 12), () => { }, "Attach an image"));
        left.Children.Add(meta);
        bar.Children.Add(left);
        send.HorizontalAlignment = HorizontalAlignment.Right;
        bar.Children.Add(send);

        suggestBox.Child = suggestions;
        suggestBox.Visibility = Visibility.Collapsed;
        suggestBox.BorderBrush = Theme.Line;
        suggestBox.BorderThickness = new Thickness(0, 0, 0, 1);
        suggestBox.Padding = new Thickness(4);
        var box = new StackPanel { Children = { suggestBox, input, bar } };
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
