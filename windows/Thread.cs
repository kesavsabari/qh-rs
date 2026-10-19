using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Qh;

/// A tab's thread, like apple/qh/ChatView.swift's ThreadView. Items are drawn once and only the one
/// that changes is redrawn (a streaming reply at most every 60 ms), so long sessions stay light.
sealed class ThreadView
{
    public readonly ScrollViewer View = new() { HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled };
    readonly StackPanel col = new() { Spacing = 16, MaxWidth = 760, Padding = new Thickness(32, 24, 32, 24) };
    readonly Dictionary<Item, Border> drawn = [];
    readonly HashSet<Item> open = [];
    Tab? tab;
    List<Item>? shown;
    Item? pending;
    readonly DispatcherTimer timer = new() { Interval = TimeSpan.FromMilliseconds(60) };

    public ThreadView()
    {
        View.Content = col;
        timer.Tick += (_, _) => { timer.Stop(); if (pending != null) { Redraw(pending); pending = null; } };
    }

    /// forget what's drawn (the theme changed): the next Show draws again
    public void Reset() { tab = null; shown = null; }

    public void Show(Tab t)
    {
        if (tab == t && shown == t.Items && drawn.Count == t.Items.Count) return;
        tab = t;
        shown = t.Items;
        col.Children.Clear();
        drawn.Clear();
        foreach (var i in t.Items) Append(i);
        Bottom();
    }

    public void Changed(Tab t, Item i, bool added)
    {
        if (t != tab) return;
        if (added) { Enter(Append(i)); Bottom(); return; }
        // streamed text: coalesce chunks into one redraw
        if (i.K == Item.Kind.Reply) { pending = i; if (!timer.IsEnabled) timer.Start(); }
        else Redraw(i);
    }

    Border Append(Item i)
    {
        var holder = new Border { Child = Draw(i) };
        drawn[i] = holder;
        col.Children.Add(holder);
        return holder;
    }

    /// a new item fades and slides up into place: two animations, 180 ms, nothing on a redraw
    static void Enter(Border b)
    {
        var move = new TranslateTransform { Y = 8 };
        b.RenderTransform = move;
        b.Opacity = 0;
        var ease = new Microsoft.UI.Xaml.Media.Animation.CubicEase { EasingMode = Microsoft.UI.Xaml.Media.Animation.EasingMode.EaseOut };
        var dur = new Duration(TimeSpan.FromMilliseconds(180));
        var fade = new Microsoft.UI.Xaml.Media.Animation.DoubleAnimation { From = 0, To = 1, Duration = dur, EasingFunction = ease };
        var slide = new Microsoft.UI.Xaml.Media.Animation.DoubleAnimation { From = 8, To = 0, Duration = dur, EasingFunction = ease };
        Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTarget(fade, b);
        Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTargetProperty(fade, "Opacity");
        Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTarget(slide, move);
        Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTargetProperty(slide, "Y");
        var sb = new Microsoft.UI.Xaml.Media.Animation.Storyboard { Children = { fade, slide } };
        sb.Completed += (_, _) => { b.Opacity = 1; b.RenderTransform = null; };
        sb.Begin();
    }

    void Redraw(Item i)
    {
        if (!drawn.TryGetValue(i, out var h)) return;
        var stick = View.VerticalOffset >= View.ScrollableHeight - 40;
        h.Child = Draw(i);
        if (stick) Bottom();
    }

    void Bottom() => View.DispatcherQueue.TryEnqueue(() => { View.UpdateLayout(); View.ChangeView(null, View.ScrollableHeight, null, true); });

    UIElement Draw(Item i) => i.K switch
    {
        Item.Kind.User => new Border
        {
            Background = Theme.Tile, CornerRadius = new CornerRadius(12), Padding = new Thickness(14, 10, 14, 10),
            HorizontalAlignment = HorizontalAlignment.Right, MaxWidth = 560,
            Child = new TextBlock { Text = i.Text, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Ink, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true, LineHeight = 22 },
        },
        Item.Kind.Reply => Markdown.Render(i.Text),
        Item.Kind.Tool => Tool(i),
        Item.Kind.Diff => Line($"{i.Text}", $"+{i.Added} −{i.Removed}", Theme.Accent, ""),
        Item.Kind.Error => new TextBlock { Text = i.Text, FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Accent, TextWrapping = TextWrapping.Wrap },
        _ => new TextBlock { Text = i.Text, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap },
    };

    /// A tool call as one quiet line; click it for the full output.
    UIElement Tool(Item i)
    {
        var state = i.Ok == null ? "…" : i.Ok == true ? "" : "failed";
        var head = Line($"{i.Name}  {i.Text}", i.Summary.Length > 0 ? i.Summary : state, i.Ok == false ? Theme.Accent : Theme.Faint, i.Ok == null ? "" : "");
        if (i.Output.Length == 0) return head;
        var box = new StackPanel { Spacing = 6 };
        box.Children.Add(head);
        if (open.Contains(i))
            box.Children.Add(new Border
            {
                Background = Theme.Tile, CornerRadius = new CornerRadius(8), Padding = new Thickness(12, 8, 12, 8), MaxHeight = 320,
                Child = new ScrollViewer { Content = new TextBlock { Text = i.Output, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Soft, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true } },
            });
        head.Tapped += (_, _) => { if (!open.Remove(i)) open.Add(i); Redraw(i); };
        return box;
    }

    static FrameworkElement Line(string what, string right, Brush rightColour, string glyph)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = glyph, FontSize = 11, Foreground = Theme.Faint });
        var w = new TextBlock { Text = what, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Soft, TextTrimming = TextTrimming.CharacterEllipsis };
        Grid.SetColumn(w, 1);
        g.Children.Add(w);
        var r = new TextBlock { Text = right, FontFamily = Theme.Sans, FontSize = 12, Foreground = rightColour, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = 280 };
        Grid.SetColumn(r, 2);
        g.Children.Add(r);
        return g;
    }
}

/// Permission cards and the setup screen.
static class Cards
{
    /// The quiet permission card: the exact command or edit, Allow once / Always / Deny.
    public static FrameworkElement Ask(Ask a, Project p)
    {
        var col = new StackPanel { Spacing = 10 };
        col.Children.Add(new TextBlock
        {
            Text = a.Guarded ? $"qh wants to {a.Tool} (outside the usual rules)" : $"qh wants to {a.Tool}",
            FontFamily = Theme.SansMedium, FontSize = 13, Foreground = Theme.Ink,
        });
        if (a.Warn.Length > 0) col.Children.Add(new TextBlock { Text = a.Warn, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = Theme.Accent, TextWrapping = TextWrapping.Wrap });
        col.Children.Add(new Border
        {
            Background = Theme.Tile, CornerRadius = new CornerRadius(8), Padding = new Thickness(12, 8, 12, 8), MaxHeight = 220,
            Child = new ScrollViewer { Content = new TextBlock { Text = a.Detail, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Ink, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true } },
        });
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        buttons.Children.Add(Button("Allow once", "Ctrl+Enter", Theme.Ink, Theme.Card, () => p.Answer(a, "yes")));
        if (a.Always.Length > 0) buttons.Children.Add(Button($"Always for {a.Always}", "", Theme.Card, Theme.Ink, () => p.Answer(a, "always")));
        buttons.Children.Add(Button("Deny", "Ctrl+Backspace", null, Theme.Soft, () => p.Answer(a, "no")));
        col.Children.Add(buttons);
        return new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(12),
            Padding = new Thickness(16, 14, 16, 14), Margin = new Thickness(0, 0, 0, 10), MaxWidth = 760, Child = col,
        };
    }

    public static Border Button(string title, string key, Brush? fill, Brush text, Action action)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        row.Children.Add(new TextBlock { Text = title, FontFamily = Theme.SansMedium, FontSize = 12.5, Foreground = text });
        if (key.Length > 0) row.Children.Add(new TextBlock { Text = key, FontFamily = Theme.Sans, FontSize = 11, Foreground = text, Opacity = 0.55 });
        var b = new Border
        {
            Background = fill ?? new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderBrush = Theme.Line, BorderThickness = new Thickness(fill == Theme.Card ? 1 : 0),
            CornerRadius = new CornerRadius(8), Padding = new Thickness(12, 6, 12, 6), Child = row,
        };
        b.Tapped += (_, _) => action();
        return b;
    }

    /// Connect a model: local Ollama, an API key, or ChatGPT sign-in; then pick the default.
    public static FrameworkElement Setup(Project p)
    {
        var col = new StackPanel { Spacing = 14, MaxWidth = 560, Padding = new Thickness(32, 40, 32, 32) };
        col.Children.Add(new TextBlock { Text = "Connect a model", FontFamily = Theme.SansMedium, FontSize = 20, Foreground = Theme.Ink });
        col.Children.Add(new TextBlock { Text = "qh runs on your own models: a local one, an API key, or your ChatGPT plan.", FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Soft, TextWrapping = TextWrapping.Wrap });
        foreach (var s in p.Sources)
        {
            var row = new StackPanel { Spacing = 8 };
            var head = new Grid();
            head.Children.Add(new TextBlock { Text = s.Label, FontFamily = Theme.SansMedium, FontSize = 13.5, Foreground = Theme.Ink });
            head.Children.Add(new TextBlock { Text = s.Connected ? (s.Account.Length > 0 ? s.Account : "connected") : "", FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Accent, HorizontalAlignment = HorizontalAlignment.Right });
            row.Children.Add(head);
            if (s.Note.Length > 0) row.Children.Add(new TextBlock { Text = s.Note, FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap });
            if (s.Kind == "key" && !s.Connected)
            {
                var key = new PasswordBox { PlaceholderText = $"API key{(s.Site.Length > 0 ? " from " + s.Site : "")}", FontFamily = Theme.Mono, FontSize = 12 };
                var id = s.Id;
                key.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Enter && key.Password.Length > 0) p.ConnectKey(id, key.Password); };
                row.Children.Add(key);
            }
            else if (s.Kind == "signin" && !s.Connected)
                row.Children.Add(Button("Sign in with ChatGPT", "", Theme.Ink, Theme.Card, p.SignInChatGpt));
            col.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(14, 12, 14, 12), Child = row });
        }
        if (p.SetupMessage.Length > 0)
            col.Children.Add(new TextBlock { Text = p.SetupMessage, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = p.SetupError ? Theme.Accent : Theme.Soft, TextWrapping = TextWrapping.Wrap });
        if (p.Models.Count > 0)
        {
            col.Children.Add(new TextBlock { Text = "Default model", FontFamily = Theme.SansMedium, FontSize = 13.5, Foreground = Theme.Ink, Margin = new Thickness(0, 8, 0, 0) });
            var list = new StackPanel { Spacing = 2 };
            foreach (var (id, from) in p.Models.Take(60))
            {
                var on = id == p.DefaultModel;
                var r = new Grid { Padding = new Thickness(10, 6, 10, 6) };
                r.Children.Add(new TextBlock { Text = id, FontFamily = Theme.Mono, FontSize = 12, Foreground = on ? Theme.Ink : Theme.Soft });
                r.Children.Add(new TextBlock { Text = from, FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right });
                var b = new Border { Child = r, CornerRadius = new CornerRadius(7), Background = on ? Theme.Tile : new SolidColorBrush(Microsoft.UI.Colors.Transparent) };
                var m = id;
                b.Tapped += (_, _) => p.SetDefault(m);
                list.Children.Add(b);
            }
            col.Children.Add(list);
        }
        if (!p.ShowSetup || p.Sources.Count == 0) { }
        return new ScrollViewer { Content = col };
    }
}
