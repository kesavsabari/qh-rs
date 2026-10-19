using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Qh;

/// W3, the Mac's M2 on Windows: review (keep / undo per file), checkpoints with rewind, the file
/// tree and a read-only code view. Same behaviour as apple/qh/ReviewView.swift, Checkpoints.swift, CodeView.swift.
static class CodeViews
{
    // MARK: review

    public static FrameworkElement Review(Project p, string? pick, Action<string> select)
    {
        if (p.Changes.Count == 0) return Empty("Nothing changed yet.", "Edits qh makes show up here to keep or undo.");
        var cur = p.Changes.FirstOrDefault(c => c.Path == pick);
        if (cur.Path == null) cur = p.Changes[0];
        var g = new Grid();
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(240) });
        g.ColumnDefinitions.Add(new ColumnDefinition());

        var list = new StackPanel { Spacing = 2, Padding = new Thickness(12, 16, 8, 16) };
        var all = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(0, 0, 0, 10) };
        all.Children.Add(Cards.Button("keep all", "", Theme.Card, Theme.Ink, () => { foreach (var c in p.Changes.ToList()) p.Accept(c.Path); }));
        all.Children.Add(Cards.Button("undo all", "", null, Theme.Soft, () => { foreach (var c in p.Changes.ToList()) p.Reject(c.Path); }));
        list.Children.Add(all);
        foreach (var c in p.Changes)
        {
            var (add, rem) = Count(c.Before, c.After);
            var row = new Grid { Padding = new Thickness(8, 5, 8, 5) };
            row.Children.Add(new TextBlock { Text = c.Path, FontFamily = Theme.Mono, FontSize = 12, Foreground = c.Path == cur.Path ? Theme.Ink : Theme.Soft, TextTrimming = TextTrimming.CharacterEllipsis, Margin = new Thickness(0, 0, 60, 0) });
            row.Children.Add(new TextBlock { Text = $"+{add} −{rem}", FontFamily = Theme.Mono, FontSize = 11, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right });
            var b = new Border { Child = row, CornerRadius = new CornerRadius(7), Background = c.Path == cur.Path ? Theme.Tile : Clear };
            var path = c.Path;
            b.Tapped += (_, _) => select(path);
            list.Children.Add(b);
        }
        g.Children.Add(new ScrollViewer { Content = list });

        var right = new Grid();
        right.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        right.RowDefinitions.Add(new RowDefinition());
        var head = new Grid { Padding = new Thickness(16, 14, 16, 10) };
        head.Children.Add(new TextBlock { Text = cur.Path, FontFamily = Theme.Mono, FontSize = 12.5, Foreground = Theme.Ink, VerticalAlignment = VerticalAlignment.Center });
        var acts = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, HorizontalAlignment = HorizontalAlignment.Right };
        var cp = cur.Path;
        acts.Children.Add(Cards.Button("undo file", "", null, Theme.Soft, () => p.Reject(cp)));
        acts.Children.Add(Cards.Button("keep", "", Theme.Ink, Theme.Card, () => p.Accept(cp)));
        head.Children.Add(acts);
        right.Children.Add(head);
        var diff = Diff(cur.Before, cur.After);
        Grid.SetRow(diff, 1);
        right.Children.Add(diff);
        Grid.SetColumn(right, 1);
        g.Children.Add(right);
        return g;
    }

    static (int, int) Count(string a, string b)
    {
        int add = 0, rem = 0;
        foreach (var (k, _) in Lines(a, b)) { if (k == '+') add++; else if (k == '-') rem++; }
        return (add, rem);
    }

    /// Line diff by longest common subsequence; big files fall back to a whole-file replace.
    static List<(char, string)> Lines(string before, string after)
    {
        var a = before.Length == 0 ? [] : before.Replace("\r\n", "\n").Split('\n');
        var b = after.Length == 0 ? [] : after.Replace("\r\n", "\n").Split('\n');
        var outp = new List<(char, string)>();
        if ((long)a.Length * b.Length > 4_000_000)
        {
            outp.AddRange(a.Select(l => ('-', l)));
            outp.AddRange(b.Select(l => ('+', l)));
            return outp;
        }
        var lcs = new int[a.Length + 1, b.Length + 1];
        for (int i = a.Length - 1; i >= 0; i--)
            for (int j = b.Length - 1; j >= 0; j--)
                lcs[i, j] = a[i] == b[j] ? lcs[i + 1, j + 1] + 1 : Math.Max(lcs[i + 1, j], lcs[i, j + 1]);
        int x = 0, y = 0;
        while (x < a.Length && y < b.Length)
        {
            if (a[x] == b[y]) { outp.Add((' ', a[x])); x++; y++; }
            else if (lcs[x + 1, y] >= lcs[x, y + 1]) outp.Add(('-', a[x++]));
            else outp.Add(('+', b[y++]));
        }
        while (x < a.Length) outp.Add(('-', a[x++]));
        while (y < b.Length) outp.Add(('+', b[y++]));
        return outp;
    }

    /// Unified diff, three lines of context around each change.
    static FrameworkElement Diff(string before, string after)
    {
        var lines = Lines(before, after);
        var keep = new HashSet<int>();
        for (int i = 0; i < lines.Count; i++)
            if (lines[i].Item1 != ' ') for (int k = Math.Max(0, i - 3); k <= Math.Min(lines.Count - 1, i + 3); k++) keep.Add(k);
        var col = new StackPanel { Padding = new Thickness(0, 0, 0, 16) };
        bool gap = false;
        for (int i = 0; i < lines.Count; i++)
        {
            if (!keep.Contains(i)) { if (!gap) col.Children.Add(new TextBlock { Text = "  ⋯", FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Faint, Margin = new Thickness(16, 4, 0, 4) }); gap = true; continue; }
            gap = false;
            var (k, text) = lines[i];
            col.Children.Add(new Border
            {
                Background = k == '+' ? new SolidColorBrush(Theme.Hex(Theme.Dark ? 0x1E3320u : 0xE6F4E4u)) : k == '-' ? new SolidColorBrush(Theme.Hex(Theme.Dark ? 0x3A1E1Bu : 0xFBE7E3u)) : Clear,
                Padding = new Thickness(16, 1, 16, 1),
                Child = new TextBlock { Text = $"{k} {text}", FontFamily = Theme.Mono, FontSize = 12, Foreground = k == ' ' ? Theme.Soft : Theme.Ink },
            });
        }
        return new ScrollViewer { Content = col, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollMode = ScrollMode.Auto };
    }

    // MARK: checkpoints

    public static FrameworkElement History(Project p)
    {
        if (p.Checkpoints.Count == 0) return Empty("No checkpoints yet.", "Each message you send saves the files qh is about to change, so you can go back.");
        var col = new StackPanel { Spacing = 8, MaxWidth = 760, Padding = new Thickness(32, 24, 32, 24) };
        col.Children.Add(new TextBlock { Text = "Checkpoints", FontFamily = Theme.SansMedium, FontSize = 17, Foreground = Theme.Ink, Margin = new Thickness(0, 0, 0, 6) });
        var busy = p.Current?.Busy == true;
        foreach (var (turn, prompt, files) in Enumerable.Reverse(p.Checkpoints))
        {
            var row = new StackPanel { Spacing = 6 };
            row.Children.Add(new TextBlock { Text = prompt.Length > 0 ? prompt : $"message {turn + 1}", FontFamily = Theme.Sans, FontSize = 13.5, Foreground = Theme.Ink, TextTrimming = TextTrimming.CharacterEllipsis, MaxLines = 2, TextWrapping = TextWrapping.Wrap });
            row.Children.Add(new TextBlock { Text = files.Count == 0 ? "no files changed" : string.Join("  ", files.Take(6)) + (files.Count > 6 ? $"  +{files.Count - 6}" : ""), FontFamily = Theme.Mono, FontSize = 11.5, Foreground = Theme.Faint, TextTrimming = TextTrimming.CharacterEllipsis });
            var acts = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            var t = turn;
            if (!busy)
            {
                acts.Children.Add(Cards.Button("Rewind files", "", Theme.Card, Theme.Ink, () => p.Rewind(t, false)));
                acts.Children.Add(Cards.Button("Rewind files and chat", "", null, Theme.Soft, () => p.Rewind(t, true)));
            }
            else acts.Children.Add(new TextBlock { Text = "stop the turn to rewind", FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Faint });
            row.Children.Add(acts);
            col.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(14, 12, 14, 12), Child = row });
        }
        return new ScrollViewer { Content = col };
    }

    // MARK: code

    /// The project's files as a tree (folders first), from serve's gitignore-aware list.
    public static FrameworkElement Tree(Project p)
    {
        var tv = new TreeView { SelectionMode = TreeViewSelectionMode.Single, Padding = new Thickness(4, 8, 4, 8) };
        var root = new Dictionary<string, TreeViewNode>();
        TreeViewNode Folder(string dir)
        {
            if (root.TryGetValue(dir, out var n)) return n;
            var slash = dir.LastIndexOf('/');
            n = new TreeViewNode { Content = new FileEntry(dir[(slash + 1)..], dir, true) };
            root[dir] = n;
            (slash < 0 ? tv.RootNodes : Folder(dir[..slash]).Children).Add(n);
            return n;
        }
        foreach (var f in p.Files.OrderBy(f => f.Count(c => c == '/') == 0).ThenBy(f => f, StringComparer.OrdinalIgnoreCase).Take(5000))
        {
            var slash = f.LastIndexOf('/');
            var node = new TreeViewNode { Content = new FileEntry(f[(slash + 1)..], f, false) };
            (slash < 0 ? tv.RootNodes : Folder(f[..slash]).Children).Add(node);
        }
        tv.ItemInvoked += (_, e) => { if (e.InvokedItem is TreeViewNode { Content: FileEntry { Dir: false } fe }) p.OpenFile(fe.Path); };
        return tv;
    }

    sealed record FileEntry(string Name, string Path, bool Dir) { public override string ToString() => Name; }

    /// Read-only code with line numbers; the line a link pointed at is marked and scrolled to.
    public static FrameworkElement Code(Project p)
    {
        if (p.Code is not { } c) return Empty("No file open", "Click a path in the chat, or pick one in Code (Ctrl+3).");
        var g = new Grid();
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        g.RowDefinitions.Add(new RowDefinition());
        var head = new Grid { Padding = new Thickness(16, 12, 16, 8) };
        head.Children.Add(new TextBlock { Text = c.Path, FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Soft, VerticalAlignment = VerticalAlignment.Center });
        var full = Path.Combine(p.Path, c.Path);
        head.Children.Add(new StackPanel
        {
            Orientation = Orientation.Horizontal, HorizontalAlignment = HorizontalAlignment.Right,
            Children = { Cards.Button("Open in editor", "", null, Theme.Soft, () => OpenInEditor(full, c.Line)) },
        });
        g.Children.Add(head);

        var lines = c.Text.Replace("\r\n", "\n").Split('\n');
        var nums = new TextBlock { FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Faint, TextAlignment = TextAlignment.Right, LineHeight = 19, LineStackingStrategy = LineStackingStrategy.BlockLineHeight, Text = string.Join("\n", Enumerable.Range(1, lines.Length)) };
        var body = new TextBlock { FontFamily = Theme.Mono, FontSize = 12, Foreground = Theme.Ink, IsTextSelectionEnabled = true, LineHeight = 19, LineStackingStrategy = LineStackingStrategy.BlockLineHeight, Text = string.Join("\n", lines) };
        var rows = new Grid { Padding = new Thickness(0, 0, 16, 16), ColumnSpacing = 14 };
        rows.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(52) });
        rows.ColumnDefinitions.Add(new ColumnDefinition());
        if (c.Line is int ln && ln >= 1 && ln <= lines.Length)
        {
            var mark = new Border { Background = Theme.Tile, Height = 19, VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(0, (ln - 1) * 19, 0, 0) };
            Grid.SetColumnSpan(mark, 2);
            rows.Children.Add(mark);
        }
        rows.Children.Add(nums);
        Grid.SetColumn(body, 1);
        rows.Children.Add(body);
        var sv = new ScrollViewer { Content = rows, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollMode = ScrollMode.Auto };
        if (c.Line is int l2) sv.Loaded += (_, _) => sv.ChangeView(null, Math.Max(0, (l2 - 8) * 19), null, true);
        Grid.SetRow(sv, 1);
        g.Children.Add(sv);
        return g;
    }

    /// VS Code at the line when it's there, else whatever opens the file type.
    static void OpenInEditor(string file, int? line)
    {
        try { Process.Start(new ProcessStartInfo("code", $"--goto \"{file}:{line ?? 1}\"") { UseShellExecute = true, WindowStyle = ProcessWindowStyle.Hidden }); }
        catch { try { Process.Start(new ProcessStartInfo(file) { UseShellExecute = true }); } catch { } }
    }

    static FrameworkElement Empty(string title, string hint) => new StackPanel
    {
        Spacing = 6, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Padding = new Thickness(32),
        Children =
        {
            new TextBlock { Text = title, FontFamily = Theme.SansMedium, FontSize = 13.5, Foreground = Theme.Ink, HorizontalAlignment = HorizontalAlignment.Center },
            new TextBlock { Text = hint, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = Theme.Soft, HorizontalAlignment = HorizontalAlignment.Center, TextWrapping = TextWrapping.Wrap, TextAlignment = TextAlignment.Center },
        },
    };

    static readonly SolidColorBrush Clear = new(Microsoft.UI.Colors.Transparent);
}
