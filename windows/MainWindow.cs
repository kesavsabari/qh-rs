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
sealed partial class MainWindow : Window
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
    readonly StackPanel tabRight = new() { Orientation = Orientation.Horizontal, Spacing = 14, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 0, 150, 0) };
    List<(string Insert, string Label, string About)> sugg = [];
    string? reviewPick;
    readonly StackPanel dots = new() { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Center };
    readonly Grid palette = new() { Visibility = Visibility.Collapsed };
    readonly TextBox paletteInput = new();
    readonly StackPanel paletteList = new() { Spacing = 1 };
    List<(string Title, string Hint, Action Run)> paletteItems = [];
    int paletteSel;
    Tray? tray;
    Peek? peek;
    bool compact;
    FrameworkElement? sideBottom;
    bool active = true;
    readonly HashSet<(string, long)> toasted = [];
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
        threadView.FixVerify = () => ws.Current?.FixVerify();
        ws.ItemChanged += (t, i, added) => { threadView.Changed(t, i, added); if (added) RenderMeta(); };
        Closed += (_, _) => { tray?.Dispose(); peek?.Dispose(); Notify.Stop(); ws.Shutdown(); };
        // coming back to the window: sessions saved meanwhile (a terminal qh) show up in the sidebar
        Activated += (_, e) => { var was = active; active = e.WindowActivationState != WindowActivationState.Deactivated; if (active && !was) ws.Current?.LoadSessions(); };
        Notify.Start(ws, DispatcherQueue, Front);
        ws.AskArrived += (p, a) => { if (!active) { Notify.Ask(p, a); toasted.Add((p.Path, a.Id)); } };
        tray = new Tray(WinRT.Interop.WindowNative.GetWindowHandle(this), Front, Close);
        peek = new Peek(WinRT.Interop.WindowNative.GetWindowHandle(this), () => ws.Current);
        // the team room while it's on screen: every 3 s, or every 30 s once serve pushes changes itself
        var poll = DispatcherQueue.CreateTimer();
        poll.Interval = TimeSpan.FromSeconds(3);
        var ticks = 0;
        poll.Tick += (_, _) =>
        {
            ticks++;
            if (ws.Space != "team" || !active || ws.Current is not { } tp) return;
            if (!tp.TeamPush || ticks % 10 == 0) tp.LoadTeam();
        };
        poll.Start();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1280, 820));
        // CI screenshots: fill the screen so the corner hover lands on the window
        if (Environment.GetEnvironmentVariable("QH_APP_MAXIMIZE") == "1" && AppWindow.Presenter is OverlappedPresenter op) op.Maximize();
        Chrome();
        ThemeFlags(ws.ThemePref);
        Build();
        Render();
    }

    /// Caption (drag) = the top edge plus the card's tab-bar row; the tabs and the links on the right
    /// stay clickable (passthrough). The caption buttons sit over the corner, hidden until hovered.
    void DragRegions()
    {
        if (root.XamlRoot is not { } xr) return;
        var scale = xr.RasterizationScale;
        Windows.Graphics.RectInt32 R(FrameworkElement e, double padX = 0)
        {
            var t = e.TransformToVisual(root).TransformPoint(new Windows.Foundation.Point(0, 0));
            return new((int)((t.X - padX) * scale), (int)(t.Y * scale), (int)((e.ActualWidth + 2 * padX) * scale), (int)(e.ActualHeight * scale));
        }
        var w = (int)(root.ActualWidth * scale);
        var src = InputNonClientPointerSource.GetForWindowId(AppWindow.Id);
        var cardTop = card.TransformToVisual(root).TransformPoint(new Windows.Foundation.Point(0, 0));
        src.SetRegionRects(NonClientRegionKind.Caption,
        [
            new(0, 0, w, (int)(8 * scale)),
            new((int)(cardTop.X * scale), (int)(cardTop.Y * scale), (int)(card.ActualWidth * scale), (int)(44 * scale)),
        ]);
        src.SetRegionRects(NonClientRegionKind.Passthrough, [R(tabBar, 2), R(tabRight, 2)]);
    }

    void Front()
    {
        AppWindow.Show();
        if (AppWindow.Presenter is OverlappedPresenter { State: OverlappedPresenterState.Minimized } op) op.Restore();
        Activate();
    }

    // MARK: window chrome

    void Chrome()
    {
        ExtendsContentIntoTitleBar = true;
        AppWindow.TitleBar.PreferredHeightOption = TitleBarHeightOption.Standard;
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
        // hidden = no caption buttons at all (any colour left a notch over the card's rounded corner);
        // the border stays, so resizing and Snap still work
        if (AppWindow.Presenter is OverlappedPresenter op) op.SetBorderAndTitleBar(true, show);
        var tb = AppWindow.TitleBar;
        Color clear = Colors.Transparent;
        tb.ButtonBackgroundColor = clear;
        tb.ButtonInactiveBackgroundColor = clear;
        tb.ButtonForegroundColor = Theme.Soft.Color;
        tb.ButtonInactiveForegroundColor = Theme.Faint.Color;
        tb.ButtonHoverBackgroundColor = Theme.Hover.Color;
        tb.ButtonHoverForegroundColor = Theme.Ink.Color;
        tb.ButtonPressedBackgroundColor = Theme.Line.Color;
        DragRegions();
    }

    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);

    // MARK: layout

    void Build()
    {
        root.Background = Theme.Ground;
        // no title-bar strip: just a thin edge of ground, the card and sidebar go right up to it
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(8) });
        root.RowDefinitions.Add(new RowDefinition());
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(260) });
        root.ColumnDefinitions.Add(new ColumnDefinition());
        // the window drags from that edge and the empty part of the tab bar (see DragRegions)
        root.SizeChanged += (_, _) => { AutoCompact(); DragRegions(); };
        tabBar.SizeChanged += (_, _) => DragRegions();
        tabRight.SizeChanged += (_, _) => DragRegions();

        var side = new Grid();
        side.RowDefinitions.Add(new RowDefinition());
        side.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        side.Children.Add(new ScrollViewer { Content = sidebar });
        var bottom = bottomHolder;
        sideBottom = bottom;
        bottom.Child = SidebarBottom();
        Grid.SetRow(bottom, 1);
        side.Children.Add(bottom);
        Grid.SetRow(side, 1);
        root.Children.Add(side);

        frame.Background = Theme.Card; frame.BorderBrush = Theme.Line; frame.BorderThickness = new Thickness(0.5);
        frame.CornerRadius = new CornerRadius(12); frame.Margin = new Thickness(0, 0, 8, 8); frame.Child = card;
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

        Key(VirtualKey.K, TogglePalette);
        Key(VirtualKey.T, () => SetSpace(ws.Space == "team" ? "personal" : "team"), VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift);
        Key(VirtualKey.T, () => ws.Current?.NewTab());
        Key(VirtualKey.Number1, () => { if (ws.Current is { } p) SetLayout(p, "chat"); });
        Key(VirtualKey.Number2, () => { if (ws.Current is { } p) SetLayout(p, "split"); });
        Key(VirtualKey.Number3, () => { if (ws.Current is { } p) SetLayout(p, "code"); });
        Key(VirtualKey.D, () => { if (ws.Current is { } p) Page(p, "review"); });
        // paths in replies open in the code view
        Markdown.IsFile = s => ws.Current?.Files.Contains(s.Split(':')[0].Replace('\\', '/')) == true;
        Markdown.Open = s => ws.Current?.OpenFile(s);
        Key(VirtualKey.O, Pick);
        // Ctrl+\ (VK_OEM_5): an accelerator never sees that key, so catch it on the way down
        root.PreviewKeyDown += (_, e) =>
        {
            if ((int)e.Key != 220 || !InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down)) return;
            e.Handled = true;
            ToggleCompact();
        };
        Key((VirtualKey)188, ToggleSettings); // Ctrl+,
        Key(VirtualKey.I, StartInline);
        // the first waiting permission: Ctrl+Enter allows once, Ctrl+Backspace denies
        Key(VirtualKey.Enter, () => { if (FirstAsk() is { } a) ws.Current!.Answer(a, "yes"); });
        Key(VirtualKey.Back, () => { if (FirstAsk() is { } a) ws.Current!.Answer(a, "no"); });
        BuildPalette();
        Content = root;
    }

    Ask? FirstAsk() => ws.Current is { } p && p.Current is { } t ? p.Asks.FirstOrDefault(a => a.Tab == t.Name) : null;

    void Key(VirtualKey k, Action a, VirtualKeyModifiers mods = VirtualKeyModifiers.Control)
    {
        var acc = new KeyboardAccelerator { Key = k, Modifiers = mods };
        acc.Invoked += (_, e) => { e.Handled = true; a(); };
        root.KeyboardAccelerators.Add(acc);
    }

    FrameworkElement SidebarBottom()
    {
        var row = new Grid { Padding = new Thickness(14, 10, 14, 10) };
        // personal ⇄ team as Zen's workspace dots: the current one is a bigger ink dot
        row.Children.Add(dots);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        right.Children.Add(Plain(Icon(""), Pick, "Open folder (Ctrl+O)"));
        right.Children.Add(Plain(Icon(""), ToggleSettings, "Settings (Ctrl+,)"));
        row.Children.Add(right);
        return row;
    }

    // MARK: render (state → views; the composer stays, so typing survives updates)

    void Render()
    {
        RenderDots();
        RenderTray();
        if (ws.Space == "team")
        {
            RenderTeam();
            // nothing of the room fits in 56 px: just the project initials
            if (compact) { sidebar.Children.Clear(); RenderCompactSidebar(); }
            return;
        }
        tabRight.Visibility = Visibility.Visible;
        RenderSidebar();
        RenderTabs();
        RenderThread();
        RenderAsks();
        RenderMeta();
    }

    void RenderDots()
    {
        dots.Children.Clear();
        foreach (var (label, space) in new[] { ("Personal", "personal"), ("Team", "team") })
        {
            var on = ws.Space == space;
            var b = new Border
            {
                Width = 26, Height = 26, Background = new SolidColorBrush(Colors.Transparent),
                Child = new Microsoft.UI.Xaml.Shapes.Ellipse { Width = on ? 8 : 6, Height = on ? 8 : 6, Fill = on ? Theme.Ink : Theme.Faint },
            };
            ToolTipService.SetToolTip(b, label + " (Ctrl+Shift+T)");
            b.Tapped += (_, _) => SetSpace(space);
            dots.Children.Add(b);
        }
    }

    void SetSpace(string space)
    {
        ws.Space = space;
        if (space == "team") ws.Current?.LoadTeam();
        Render();
    }

    void RenderTray()
    {
        tray?.Update(ws.Projects.Sum(p => p.Tabs.Count(t => t.Busy)), ws.Projects.Sum(p => p.Asks.Count));
        // toasts for asks that were answered (here or elsewhere) go away
        foreach (var (path, id) in toasted.ToList())
        {
            var p = ws.Projects.FirstOrDefault(x => x.Path == path);
            if (p != null && p.Asks.Any(a => a.Id == id)) continue;
            if (p != null) Notify.Clear(p, id);
            toasted.Remove((path, id));
        }
    }

    // MARK: team space: the project's room, its people and their agents (apple/qh/TeamView.swift)

    void RenderTeam()
    {
        sidebar.Children.Clear();
        tabBar.Children.Clear();
        tabRight.Visibility = Visibility.Collapsed;
        asks.Children.Clear();
        Detach();
        var p = ws.Current;
        if (p == null || !p.TeamRunning)
        {
            sidebar.Children.Add(Label("TEAM", 11, Theme.Faint, new Thickness(16, 8, 16, 4)));
            thread.Content = new TextBlock
            {
                Text = p == null ? "Open a folder to start." : "No team in this project yet. Start one from a terminal with qh's /team, then come back here.",
                FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap,
                HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(32, 56, 32, 0),
            };
            meta.Text = "";
            metaButtons.Children.Clear();
            return;
        }
        sidebar.Children.Add(Label(p.TeamStatus.Length > 0 ? p.TeamStatus : p.Name, 12, Theme.Soft, new Thickness(16, 8, 16, 8)));
        sidebar.Children.Add(Label("CHANNELS", 11, Theme.Faint, new Thickness(16, 8, 16, 4)));
        foreach (var (id, label, unread, pinged) in p.Channels)
        {
            var g = new Grid();
            g.Children.Add(Label(label, 13, id == p.Chan ? Theme.Ink : Theme.Soft));
            if (unread > 0) g.Children.Add(new TextBlock { Text = unread.ToString(), FontFamily = Theme.Sans, FontSize = 11, Foreground = pinged ? Theme.Accent : Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right });
            var cid = id;
            sidebar.Children.Add(Row(g, id == p.Chan, () => p.LoadTeam(cid), new Thickness(8, 0, 8, 0)));
        }
        sidebar.Children.Add(Label("PEOPLE", 11, Theme.Faint, new Thickness(16, 14, 16, 4)));
        foreach (var (login, online, status) in p.People)
        {
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
            row.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 6, Height = 6, Fill = online ? Theme.Accent : Theme.Faint, VerticalAlignment = VerticalAlignment.Center });
            row.Children.Add(Label(login + (login == p.TeamMe ? " (you)" : ""), 13, Theme.Ink));
            if (status.Length > 0) row.Children.Add(Label(status, 12, Theme.Faint));
            sidebar.Children.Add(new Border { Padding = new Thickness(16, 5, 16, 5), Child = row });
        }
        if (p.Agents.Count > 0)
        {
            sidebar.Children.Add(Label("AGENTS", 11, Theme.Faint, new Thickness(16, 14, 16, 4)));
            foreach (var (login, name, status, now, waiting) in p.Agents)
            {
                var col = new StackPanel { Spacing = 1 };
                col.Children.Add(Label($"{login} · {name}", 12.5, Theme.Ink));
                col.Children.Add(Label(waiting ? "waiting for its owner" : now.Length > 0 ? now : status, 11.5, waiting ? Theme.Accent : Theme.Faint));
                sidebar.Children.Add(new Border { Padding = new Thickness(16, 4, 16, 4), Child = col });
            }
        }

        tabBar.Children.Add(new Border { Padding = new Thickness(10, 5, 10, 5), Margin = new Thickness(0, 0, 0, 6), CornerRadius = new CornerRadius(7), Background = Theme.Tile, Child = Label(p.Chan, 12.5, Theme.Ink) });
        var list = new StackPanel { Spacing = 12, MaxWidth = 760, Padding = new Thickness(32, 24, 32, 24) };
        foreach (var (from, text, q) in p.Waiting)
            list.Children.Add(new Border
            {
                Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(14, 10, 14, 10),
                Child = new TextBlock { Text = $"{from}{(q ? " asks" : "")}: {text}", FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Ink, TextWrapping = TextWrapping.Wrap },
            });
        foreach (var (who, when, text) in p.TeamMessages)
        {
            var m = new StackPanel { Spacing = 2 };
            m.Children.Add(new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Children = { Label(who, 12.5, Theme.Ink), Label(when, 11, Theme.Faint) } });
            m.Children.Add(new TextBlock { Text = text, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Ink, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
            list.Children.Add(m);
        }
        if (list.Children.Count == 0) list.Children.Add(Label("Nothing here yet. Say hi.", 13, Theme.Faint));
        var sv = new ScrollViewer { Content = list };
        sv.Loaded += (_, _) => sv.ChangeView(null, sv.ScrollableHeight, null, true);
        thread.Content = sv;
        meta.Text = $"to {p.Chan}";
        metaButtons.Children.Clear();
    }

    // MARK: Ctrl+K palette (apple/qh/Palette.swift)

    void BuildPalette()
    {
        palette.Background = new SolidColorBrush(Color.FromArgb(0x40, 0, 0, 0));
        Grid.SetRowSpan(palette, 2);
        Grid.SetColumnSpan(palette, 2);
        palette.Tapped += (_, e) => { if (e.OriginalSource == palette) TogglePalette(); };
        paletteInput.PlaceholderText = "Type a command, a tab, a file…";
        paletteInput.FontFamily = Theme.Sans;
        paletteInput.FontSize = 14;
        paletteInput.TextChanged += (_, _) => FillPalette();
        paletteInput.PreviewKeyDown += (_, e) =>
        {
            if (e.Key is VirtualKey.Down or VirtualKey.Up && paletteItems.Count > 0)
            { e.Handled = true; paletteSel = (paletteSel + (e.Key == VirtualKey.Down ? 1 : paletteItems.Count - 1)) % paletteItems.Count; DrawPalette(); }
            else if (e.Key == VirtualKey.Enter) { e.Handled = true; RunPalette(paletteSel); }
            else if (e.Key == VirtualKey.Escape) { e.Handled = true; TogglePalette(); }
        };
        palette.Children.Add(paletteCard = new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(12),
            Width = 560, VerticalAlignment = VerticalAlignment.Top, HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(0, 90, 0, 0),
            Padding = new Thickness(8), Shadow = new ThemeShadow(), Translation = new System.Numerics.Vector3(0, 0, 32),
            Child = new StackPanel { Spacing = 6, Children = { paletteInput, new ScrollViewer { MaxHeight = 380, Content = paletteList } } },
        });
        root.Children.Add(palette);
    }

    void TogglePalette()
    {
        var show = palette.Visibility == Visibility.Collapsed;
        palette.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        if (!show) { input.Focus(FocusState.Programmatic); return; }
        paletteInput.Text = "";
        FillPalette();
        paletteInput.Focus(FocusState.Programmatic);
    }

    List<(string, string, Action)> PaletteAll()
    {
        var all = new List<(string, string, Action)>();
        var p = ws.Current;
        void Act(string t, string k, Action a) => all.Add((t, k, a));
        Act("New tab", "Ctrl+T", () => p?.NewTab());
        Act("Open folder…", "Ctrl+O", Pick);
        Act(compact ? "Full sidebar" : "Compact mode", "Ctrl+\\", ToggleCompact);
        Act(ws.Space == "team" ? "Personal space" : "Team space", "Ctrl+Shift+T", () => SetSpace(ws.Space == "team" ? "personal" : "team"));
        if (p != null)
        {
            Act("Chat", "Ctrl+1", () => SetLayout(p, "chat"));
            Act("Chat and code", "Ctrl+2", () => SetLayout(p, "split"));
            Act("Code", "Ctrl+3", () => SetLayout(p, "code"));
            Act("Review changes", "Ctrl+D", () => Page(p, "review"));
            Act("Checkpoints", "", () => Page(p, "history"));
            Act("Project memory", "NOTES.md", () => { showSettings = false; Page(p, "memory"); });
            Act("Activity log", "commands, edits, network", () => { showSettings = false; Page(p, "activity"); });
            Act("Connect models", "", () => { p.ShowSetup = true; p.LoadSetup(); Render(); });
            Act("Settings", "Ctrl+,", ToggleSettings);
            foreach (var th in new[] { "system", "light", "dark" }) { var v = th; Act($"Theme: {v}", "appearance", () => { ws.ThemePref = v; ws.Save(); Retheme(v); }); }
            foreach (var t in p.Tabs) { var n = t.Name; Act($"Tab: {t.Title}", "", () => { p.CurrentTab = n; p.Page = null; Render(); }); }
            foreach (var (cmd, about) in p.Commands) { var c = cmd; Act(cmd, about, () => { input.Text = c + " "; input.SelectionStart = input.Text.Length; }); }
            foreach (var f in p.Files.Take(4000)) { var path = f; Act(f, "file", () => p.OpenFile(path)); }
        }
        foreach (var r in ws.Recent.Where(r => r != p?.Path)) { var path = r; Act($"Project: {Path.GetFileName(r.TrimEnd('\\'))}", r, () => ws.Open(path)); }
        return all;
    }

    void FillPalette()
    {
        var q = paletteInput.Text.Trim().ToLowerInvariant();
        paletteItems = PaletteAll().Select(x => (x, score: q.Length == 0 ? (x.Item2 == "file" ? 1000 : 0) : Fuzzy(x.Item1.ToLowerInvariant(), q)))
            .Where(x => x.score >= 0 && (q.Length > 0 || x.x.Item2 != "file")).OrderBy(x => x.score).Take(40).Select(x => x.x).ToList();
        paletteSel = 0;
        DrawPalette();
    }

    void DrawPalette()
    {
        paletteList.Children.Clear();
        for (int i = 0; i < paletteItems.Count; i++)
        {
            var (title, hint, _) = paletteItems[i];
            var g = new Grid { Padding = new Thickness(10, 6, 10, 6) };
            g.Children.Add(new TextBlock { Text = title, FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Ink, TextTrimming = TextTrimming.CharacterEllipsis, Margin = new Thickness(0, 0, 140, 0) });
            g.Children.Add(new TextBlock { Text = hint, FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right, MaxWidth = 200, TextTrimming = TextTrimming.CharacterEllipsis });
            var k = i;
            var b = new Border { Child = g, CornerRadius = new CornerRadius(7), Background = i == paletteSel ? Theme.Tile : new SolidColorBrush(Colors.Transparent) };
            b.Tapped += (_, _) => RunPalette(k);
            paletteList.Children.Add(b);
        }
    }

    void RunPalette(int i)
    {
        if (i >= paletteItems.Count) return;
        var run = paletteItems[i].Run;
        TogglePalette();
        run();
    }

    void RenderMeta()
    {
        if (ws.Space == "team") return;
        var t = ws.Current?.Current;
        var bits = t == null ? [] : new List<string> { Theme.ShortModel(t.Model, t.Provider), ws.Current!.Mode };
        if (t is { Tokens: >= 1000 }) bits.Add($"{t.Tokens / 1000}k tokens");
        if (t?.Cost is > 0) bits.Add($"${t.Cost:0.000}");
        meta.Text = string.Join("   ", bits.Skip(2).Where(s => s.Length > 0));
        RenderMetaButtons(t);
        var busy = t?.Busy == true;
        send.Background = busy ? Theme.Accent : input.Text.Length > 0 || attached.Count > 0 ? Theme.Ink : Theme.Faint;
        ((FontIcon)send.Child).Glyph = busy ? "\uE71A" : "\uE74A";
    }

    void RenderAsks()
    {
        asks.Children.Clear();
        if (ws.Current is not { } p || p.Current is not { } t) return;
        foreach (var a in p.Asks.Where(a => a.Tab == t.Name)) asks.Children.Add(Cards.Ask(a, p));
        if (t.Plan.Count > 0 || t.PlanReady) asks.Children.Add(PlanCard(p, t));
    }

    // MARK: compact mode: the sidebar shrinks to a column of project initials, the card fills the rest.
    // Ctrl+\ sets it explicitly (remembered); otherwise it follows the width (on under ~900 px).

    void ToggleCompact() { ws.Compact = !compact; SetCompact(!compact); }

    void AutoCompact()
    {
        var want = ws.Compact ?? root.ActualWidth < 900;
        if (want != compact) SetCompact(want);
    }

    void SetCompact(bool on)
    {
        compact = on;
        root.ColumnDefinitions[0].Width = new GridLength(on ? 56 : 260);
        if (sideBottom != null) sideBottom.Visibility = on ? Visibility.Collapsed : Visibility.Visible;
        Render();
        // the card moved: the caption rects follow it once layout has run
        DispatcherQueue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, DragRegions);
    }

    void RenderCompactSidebar()
    {
        var col = new StackPanel { Spacing = 6, Margin = new Thickness(8, 4, 0, 0) };
        foreach (var path in ws.Recent)
        {
            bool on = ws.Current?.Path == path;
            var name = Path.GetFileName(path.TrimEnd('\\'));
            var b = new Border
            {
                Width = 40, Height = 40, CornerRadius = new CornerRadius(10), Background = on ? Theme.Card : Theme.Tile,
                Child = new TextBlock
                {
                    Text = name.Length > 0 ? name[..1].ToUpperInvariant() : "?", FontFamily = on ? Theme.SansMedium : Theme.Sans, FontSize = 14,
                    Foreground = on ? Theme.Ink : Theme.Soft, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center,
                },
            };
            if (on) b.Shadow = new ThemeShadow();
            Hover(b, on ? Theme.Card : Theme.Tile);
            var p = path;
            b.Tapped += (_, _) => ws.Open(p);
            ToolTipService.SetToolTip(b, path);
            col.Children.Add(b);
        }
        var open = Plain(Icon("\uE8B7"), Pick, "Open folder (Ctrl+O)");
        open.HorizontalAlignment = HorizontalAlignment.Left;
        open.Margin = new Thickness(8, 0, 0, 0);
        col.Children.Add(open);
        sidebar.Children.Add(col);
    }

    void RenderSidebar()
    {
        sidebar.Children.Clear();
        if (compact) { RenderCompactSidebar(); return; }
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
        RenderSessions(cur);
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
        tabRight.Children.Add(LayoutSwitch(p));
        tabRight.Children.Add(MoreButton(p));
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
        if (p.Page == "memory") p.LoadMemory();
        if (p.Page == "activity") p.LoadActivity();
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
        if (showSettings) { thread.Content = SettingsPage.Build(ws, v => Retheme(v), OpenModels, ToggleSettings, Render); return; }
        if (p is { ShowSetup: true, State: "ready" }) { thread.Content = Cards.Setup(p); return; }
        if (p is { State: "ready" })
        {
            if (p.Draft.Length > 0) { input.Text = p.Draft; input.SelectionStart = input.Text.Length; p.Draft = ""; }
            if (p.Page == "review") { thread.Content = CodeViews.Review(p, reviewPick, path => { reviewPick = path; Render(); }); return; }
            if (p.Page == "history") { thread.Content = CodeViews.History(p); return; }
            if (p.Page == "memory") { thread.Content = Pages.Memory(p, () => { p.Page = null; Render(); }); return; }
            if (p.Page == "activity") { thread.Content = Pages.Activity(p, Render, () => { p.Page = null; Render(); }); return; }
            if (p.Layout == "code") { thread.Content = Columns(CodeViews.Tree(p), CodeViews.Code(p, Render), 260); return; }
        }
        string? hint = ws.Qh == null ? "qh isn't installed. Install it with install.ps1, then reopen."
            : p == null ? "Open a folder to start."
            : p.State.StartsWith("closed") ? p.State[8..]
            : p.State == "starting" ? $"starting qh in {p.Name}…"
            : p.Current is not { } t ? "Ask qh anything about this project." : null;
        if (hint == null && p!.Current!.Items.Count == 0)
        {
            thread.Content = EmptyChat.Build(p.Name, s => { input.Text = s; input.SelectionStart = s.Length; input.Focus(FocusState.Programmatic); });
            return;
        }
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
        thread.Content = p.Layout == "split" && p.Code != null ? Columns(threadView.View, CodeViews.Code(p, Render), 0) : threadView.View;
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
            var ctrl = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
            // Ctrl+Enter / Ctrl+Backspace answer the waiting permission, never send or delete a word
            if (ctrl && e.Key is VirtualKey.Enter or VirtualKey.Back)
            {
                if (FirstAsk() is { } fa) { e.Handled = true; ws.Current!.Answer(fa, e.Key == VirtualKey.Enter ? "yes" : "no"); }
                else if (e.Key == VirtualKey.Enter) e.Handled = true;
                return;
            }
            if (sugg.Count > 0 && e.Key is VirtualKey.Down or VirtualKey.Up)
            { e.Handled = true; suggSel = (suggSel + (e.Key == VirtualKey.Down ? 1 : sugg.Count - 1)) % sugg.Count; DrawSuggestions(); }
            else if (sugg.Count > 0 && (e.Key == VirtualKey.Tab || (e.Key == VirtualKey.Enter && !shift)))
            { e.Handled = true; Accept(); }
            else if (e.Key == VirtualKey.Enter && !shift) { e.Handled = true; Send(); }
            else if (e.Key == VirtualKey.Escape && sugg.Count > 0) { e.Handled = true; sugg = []; DrawSuggestions(); }
            else if (e.Key == VirtualKey.Escape && showSettings) { e.Handled = true; ToggleSettings(); }
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
        attachIcon = Icon("", 12);
        left.Children.Add(Plain(attachIcon, PickImages, "Attach an image (or paste one)"));
        left.Children.Add(metaButtons);
        left.Children.Add(meta);
        left.Spacing = 6;
        input.Paste += PasteImage;
        bar.Children.Add(left);
        send.HorizontalAlignment = HorizontalAlignment.Right;
        bar.Children.Add(send);

        suggestBox.Child = suggestions;
        suggestBox.Visibility = Visibility.Collapsed;
        suggestBox.BorderBrush = Theme.Line;
        suggestBox.BorderThickness = new Thickness(0, 0, 0, 1);
        suggestBox.Padding = new Thickness(4);
        var box = new StackPanel { Children = { suggestBox, chips, input, bar } };
        return composerCard = new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(12),
            Margin = new Thickness(0, 0, 0, 16), MaxWidth = 760, HorizontalAlignment = HorizontalAlignment.Stretch, Padding = new Thickness(4, 4, 4, 0),
            Child = box,
        };
    }

    void Send()
    {
        if (ws.Current is not { } p || (input.Text.Trim().Length == 0 && attached.Count == 0)) return;
        if (ws.Space == "team") { if (p.TeamRunning && input.Text.Trim().Length > 0) p.TeamSend(input.Text.Trim()); input.Text = ""; return; }
        p.Prompt(input.Text, attached.Select(a => a.Data).ToList());
        input.Text = "";
        attached.Clear();
        DrawChips();
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
