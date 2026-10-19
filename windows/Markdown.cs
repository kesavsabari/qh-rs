using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;

namespace Qh;

/// Just enough markdown for replies: headings, lists, quotes, fenced code (with copy), and inline
/// `code`, **bold** and *italic*. Lean on purpose: a reply re-renders on every streamed chunk.
static class Markdown
{
    public static StackPanel Render(string md)
    {
        var col = new StackPanel { Spacing = 10 };
        var lines = md.Replace("\r\n", "\n").Split('\n');
        var para = new List<string>();
        void Flush() { if (para.Count > 0) { col.Children.Add(Text(string.Join(" ", para))); para.Clear(); } }

        for (int i = 0; i < lines.Length; i++)
        {
            var l = lines[i];
            var t = l.TrimStart();
            if (t.StartsWith("```"))
            {
                Flush();
                var code = new List<string>();
                for (i++; i < lines.Length && !lines[i].TrimStart().StartsWith("```"); i++) code.Add(lines[i]);
                col.Children.Add(Code(string.Join("\n", code)));
            }
            else if (t.Length == 0) Flush();
            else if (t.StartsWith('#'))
            {
                Flush();
                int level = t.TakeWhile(c => c == '#').Count();
                var h = Text(t[level..].Trim(), 16.5 - level, true);
                h.Margin = new Thickness(0, 6, 0, 0);
                col.Children.Add(h);
            }
            else if (t.StartsWith("- ") || t.StartsWith("* ") || t.StartsWith("• ") || NumberedItem(t, out _))
            {
                Flush();
                string mark, body;
                if (NumberedItem(t, out var n)) { mark = n + "."; body = t[(t.IndexOf(". ") + 2)..]; }
                else { mark = "•"; body = t[2..]; }
                var indent = (l.Length - t.Length) / 2 * 16;
                var row = new Grid { Margin = new Thickness(indent, 0, 0, 0), ColumnSpacing = 8 };
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(18) });
                row.ColumnDefinitions.Add(new ColumnDefinition());
                row.Children.Add(new TextBlock { Text = mark, FontFamily = Theme.Sans, FontSize = 14, Foreground = Theme.Soft, HorizontalAlignment = HorizontalAlignment.Right });
                var b = Text(body);
                Grid.SetColumn(b, 1);
                row.Children.Add(b);
                col.Children.Add(row);
            }
            else if (t.StartsWith("> "))
            {
                Flush();
                col.Children.Add(new Border { BorderBrush = Theme.Line, BorderThickness = new Thickness(2, 0, 0, 0), Padding = new Thickness(12, 0, 0, 0), Child = Text(t[2..]) });
            }
            else para.Add(t);
        }
        Flush();
        return col;
    }

    static bool NumberedItem(string t, out string n)
    {
        var dot = t.IndexOf(". ");
        n = dot > 0 ? t[..dot] : "";
        return dot is > 0 and < 4 && n.All(char.IsDigit);
    }

    static RichTextBlock Text(string s, double size = 14, bool bold = false)
    {
        var p = new Paragraph();
        Inline(p.Inlines, s);
        var r = new RichTextBlock
        {
            FontFamily = bold ? Theme.SansMedium : Theme.Sans, FontSize = size, Foreground = Theme.Ink,
            LineHeight = size * 1.6, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.Wrap,
        };
        r.Blocks.Add(p);
        return r;
    }

    /// `code`, **bold**, *italic*; everything else as typed.
    static void Inline(InlineCollection into, string s)
    {
        int i = 0;
        var plain = new System.Text.StringBuilder();
        void Plain() { if (plain.Length > 0) { into.Add(new Run { Text = plain.ToString() }); plain.Clear(); } }
        while (i < s.Length)
        {
            if (s[i] == '`' && s.IndexOf('`', i + 1) is var e && e > i)
            {
                Plain();
                into.Add(new Run { Text = s[(i + 1)..e], FontFamily = Theme.Mono, FontSize = 13, Foreground = Theme.Accent });
                i = e + 1;
            }
            else if (s.AsSpan(i).StartsWith("**") && s.IndexOf("**", i + 2) is var b && b > i)
            {
                Plain();
                into.Add(new Run { Text = s[(i + 2)..b], FontFamily = Theme.SansMedium });
                i = b + 2;
            }
            else if (s[i] == '*' && i + 1 < s.Length && s[i + 1] != ' ' && s.IndexOf('*', i + 1) is var it && it > i)
            {
                Plain();
                into.Add(new Run { Text = s[(i + 1)..it], FontStyle = Windows.UI.Text.FontStyle.Italic });
                i = it + 1;
            }
            else plain.Append(s[i++]);
        }
        Plain();
    }

    static Border Code(string code)
    {
        var text = new TextBlock { Text = code, FontFamily = Theme.Mono, FontSize = 12.5, Foreground = Theme.Ink, IsTextSelectionEnabled = true, LineHeight = 19 };
        var copy = new TextBlock { Text = "copy", FontFamily = Theme.Sans, FontSize = 11, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Top };
        copy.Tapped += (_, _) =>
        {
            var d = new Windows.ApplicationModel.DataTransfer.DataPackage();
            d.SetText(code);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(d);
            copy.Text = "copied";
        };
        var g = new Grid();
        g.Children.Add(new ScrollViewer { Content = text, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, HorizontalScrollMode = ScrollMode.Auto });
        g.Children.Add(copy);
        return new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(8), Padding = new Thickness(14, 10, 14, 10), Child = g };
    }
}
