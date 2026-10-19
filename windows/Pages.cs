using Microsoft.UI;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.System;

namespace Qh;

/// The ⋯ menu's pages: project memory (NOTES.md) and the activity log. Like apple/qh/CoreUI.swift.
static class Pages
{
    static int shownVersion = -1;
    static Project? shownFor;
    static string draft = "";

    static StackPanel Column(string title, string about, Action close)
    {
        var col = new StackPanel { Spacing = 14, MaxWidth = 760, Padding = new Thickness(32, 24, 32, 32) };
        var head = new Grid();
        head.Children.Add(new TextBlock { Text = title, FontFamily = Theme.SansMedium, FontSize = 17, Foreground = Theme.Ink });
        var x = new Border
        {
            Child = new TextBlock { Text = "Esc", FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint },
            Padding = new Thickness(6, 2, 6, 2), HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center, Background = new SolidColorBrush(Colors.Transparent),
        };
        x.Tapped += (_, _) => close();
        head.Children.Add(x);
        col.Children.Add(head);
        if (about.Length > 0) col.Children.Add(new TextBlock { Text = about, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = Theme.Soft, TextWrapping = TextWrapping.Wrap });
        return col;
    }

    static ScrollViewer Wrap(StackPanel col, Action close)
    {
        var sv = new ScrollViewer { Content = col };
        sv.KeyDown += (_, e) => { if (e.Key == VirtualKey.Escape) { e.Handled = true; close(); } };
        return sv;
    }

    // MARK: memory

    public static FrameworkElement Memory(Project p, Action close)
    {
        // what's being typed survives redraws; a fresh "memory" from serve replaces it
        if (shownVersion != p.MemoryVersion || shownFor != p) { draft = p.MemoryText; shownVersion = p.MemoryVersion; shownFor = p; }
        Action? save = null;
        var notes = new TextBox
        {
            AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, FontSize = 12.5, MinHeight = 260, MaxHeight = 520, Text = draft,
            FontFamily = Theme.Mono, PlaceholderText = "Nothing remembered yet. What qh should know about this project, one note per line.",
        };
        notes.TextChanged += (_, _) => draft = notes.Text;
        notes.KeyDown += (_, e) =>
        {
            if (e.Key == VirtualKey.S && InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down))
            { e.Handled = true; save?.Invoke(); }
        };
        var col = Column("Project memory", "What qh remembers about this project (NOTES.md). It reads this at the start of every chat.", close);
        var status = new TextBlock { FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Faint, VerticalAlignment = VerticalAlignment.Center };
        save = () => { p.SaveMemory(notes.Text); status.Text = "saved"; };
        col.Children.Add(new TextBlock { Text = p.MemoryPath, FontFamily = Theme.Mono, FontSize = 11.5, Foreground = Theme.Faint, TextTrimming = TextTrimming.CharacterEllipsis });
        col.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(4), Child = notes });
        col.Children.Add(new StackPanel
        {
            Orientation = Orientation.Horizontal, Spacing = 10,
            Children = { Cards.Button("Save", "Ctrl+S", Theme.Ink, Theme.Card, () => save?.Invoke()), status },
        });
        if (p.Instructions.Count > 0)
        {
            col.Children.Add(new TextBlock { Text = "Instruction files (read-only)", FontFamily = Theme.SansMedium, FontSize = 13.5, Foreground = Theme.Ink, Margin = new Thickness(0, 12, 0, 0) });
            foreach (var (path, text) in p.Instructions)
            {
                var box = new StackPanel { Spacing = 6 };
                box.Children.Add(new TextBlock { Text = path, FontFamily = Theme.Mono, FontSize = 11.5, Foreground = Theme.Soft, TextTrimming = TextTrimming.CharacterEllipsis });
                box.Children.Add(new ScrollViewer
                {
                    MaxHeight = 260,
                    Content = new TextBlock { Text = text, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Soft, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true },
                });
                col.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(14, 10, 14, 10), Child = box });
            }
        }
        return Wrap(col, close);
    }

    // MARK: activity

    static string filter = "All";

    public static FrameworkElement Activity(Project p, Action redraw, Action close)
    {
        var col = Column("Activity", "What qh did in this chat: commands run, files edited, network asks, approvals. Newest first.", close);
        var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, Padding = new Thickness(2) };
        foreach (var f in new[] { "All", "Commands", "Edits", "Network", "Approvals" })
        {
            var on = f == filter;
            var n = f == "All" ? p.Activity.Count : p.Activity.Count(a => a.Group == f);
            var b = new Border
            {
                CornerRadius = new CornerRadius(6), Padding = new Thickness(10, 3, 10, 3), Background = on ? Theme.Card : new SolidColorBrush(Colors.Transparent),
                Child = new TextBlock { Text = n > 0 ? $"{f}  {n}" : f, FontFamily = Theme.Sans, FontSize = 12, Foreground = on ? Theme.Ink : Theme.Soft },
            };
            var ff = f;
            b.Tapped += (_, _) => { filter = ff; redraw(); };
            chips.Children.Add(b);
        }
        col.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(8), Child = chips, HorizontalAlignment = HorizontalAlignment.Left });
        var rows = p.Activity.Where(a => filter == "All" || a.Group == filter).ToList();
        if (rows.Count == 0)
            col.Children.Add(new TextBlock { Text = "Nothing here yet.", FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Faint, Margin = new Thickness(0, 12, 0, 0) });
        var list = new StackPanel { Spacing = 0 };
        foreach (var a in rows)
        {
            var g = new Grid { ColumnSpacing = 12, Padding = new Thickness(4, 7, 4, 7) };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(64) });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(78) });
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.Children.Add(new TextBlock { Text = a.T.ToString("HH:mm:ss"), FontFamily = Theme.Mono, FontSize = 11.5, Foreground = Theme.Faint });
            var k = new TextBlock { Text = a.Group, FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Soft };
            Grid.SetColumn(k, 1); g.Children.Add(k);
            var t = new TextBlock { Text = a.Text, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Ink, TextTrimming = TextTrimming.CharacterEllipsis, IsTextSelectionEnabled = true };
            ToolTipService.SetToolTip(t, a.Text);
            Grid.SetColumn(t, 2); g.Children.Add(t);
            var ok = new TextBlock { Text = a.Ok ? "✓" : "✗", FontFamily = Theme.Sans, FontSize = 12, Foreground = a.Ok ? Theme.Faint : Theme.Accent };
            Grid.SetColumn(ok, 3); g.Children.Add(ok);
            list.Children.Add(new Border { BorderBrush = Theme.Line, BorderThickness = new Thickness(0, 0, 0, 1), Child = g });
        }
        col.Children.Add(list);
        return Wrap(col, close);
    }
}

/// The plan card, saved sessions in the sidebar, and starting an inline edit.
sealed partial class MainWindow
{
    readonly TextBox planEdits = new() { PlaceholderText = "Changes to the plan (optional)", AcceptsReturn = false, FontSize = 12.5 };

    FrameworkElement PlanCard(Project p, Tab t)
    {
        var col = new StackPanel { Spacing = 6 };
        col.Children.Add(new TextBlock { Text = t.PlanReady ? "Plan ready" : "Plan", FontFamily = Theme.SansMedium, FontSize = 13, Foreground = Theme.Ink });
        foreach (var (text, status) in t.Plan)
        {
            var done = status is "done" or "completed";
            var mark = done ? "☑" : status is "in_progress" or "active" ? "◐" : "☐";
            col.Children.Add(new StackPanel
            {
                Orientation = Orientation.Horizontal, Spacing = 8,
                Children =
                {
                    new TextBlock { Text = mark, FontFamily = Theme.Sans, FontSize = 13, Foreground = done ? Theme.Faint : Theme.Soft },
                    new TextBlock { Text = text, FontFamily = Theme.Sans, FontSize = 13, Foreground = done ? Theme.Faint : Theme.Ink, TextWrapping = TextWrapping.Wrap,
                        TextDecorations = done ? Windows.UI.Text.TextDecorations.Strikethrough : Windows.UI.Text.TextDecorations.None },
                },
            });
        }
        if (t.PlanReady)
        {
            if (planEdits.Parent is Panel old) old.Children.Remove(planEdits);
            planEdits.FontFamily = Theme.Sans;
            planEdits.Margin = new Thickness(0, 6, 0, 0);
            col.Children.Add(planEdits);
            void Go() { var e = planEdits.Text; planEdits.Text = ""; p.ApprovePlan(e); }
            planEdits.KeyDown -= PlanKey;
            planEdits.KeyDown += PlanKey;
            planGo = Go;
            col.Children.Add(new StackPanel
            {
                Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0),
                Children = { Cards.Button("Approve", "", Theme.Ink, Theme.Card, Go) },
            });
        }
        return new Border
        {
            Background = Theme.Card, BorderBrush = Theme.Line, BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(12),
            Padding = new Thickness(16, 12, 16, 12), Margin = new Thickness(0, 0, 0, 10), MaxWidth = 760, Child = col,
        };
    }

    Action? planGo;
    void PlanKey(object s, KeyRoutedEventArgs e) { if (e.Key == VirtualKey.Enter) { e.Handled = true; planGo?.Invoke(); } }

    /// Saved sessions under the open tabs, newest first; the open ones are marked, a click resumes.
    void RenderSessions(Project cur)
    {
        if (cur.Sessions.Count == 0) return;
        sidebar.Children.Add(Label("SESSIONS", 11, Theme.Faint, new Thickness(16, 16, 16, 4)));
        foreach (var s in cur.Sessions.Take(30))
        {
            var openTab = cur.OpenSessions.GetValueOrDefault(s.Id);
            var g = new Grid { ColumnSpacing = 8 };
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.Children.Add(Label(s.Title, 12.5, openTab != null ? Theme.Ink : Theme.Soft));
            var when = Label(Ago(s.When), 11, Theme.Faint);
            when.VerticalAlignment = VerticalAlignment.Center;
            Grid.SetColumn(when, 1);
            g.Children.Add(when);
            var id = s.Id;
            var row = Row(g, openTab != null && openTab == cur.Current?.Name, () => cur.Resume(id), new Thickness(8, 0, 8, 0));
            ToolTipService.SetToolTip(row, $"{s.Title}\n{s.Messages} messages · {s.When:g}");
            sidebar.Children.Add(row);
        }
    }

    static string Ago(DateTimeOffset t)
    {
        var d = DateTimeOffset.Now - t;
        return d.TotalMinutes < 1 ? "now" : d.TotalHours < 1 ? $"{(int)d.TotalMinutes}m" : d.TotalDays < 1 ? $"{(int)d.TotalHours}h" : d.TotalDays < 30 ? $"{(int)d.TotalDays}d" : t.ToString("MMM d");
    }

    /// Ctrl+I in the code view: an instruction bar for the picked lines (or the marked one).
    void StartInline()
    {
        if (ws.Current is not { Code: { } c } p) return;
        var sel = p.Selection ?? (c.Line ?? 1, c.Line ?? 1);
        p.Selection = sel;
        p.Inline = new InlineEdit { Path = c.Path, Start = sel.Start, End = sel.End };
        if (p.Layout == "chat") p.Layout = "split";
        p.Page = null;
        Render();
    }
}
