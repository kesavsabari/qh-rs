using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Qh;

/// The gear's page, in the card: appearance, default layout, reopen the last project, models.
static class SettingsPage
{
    public static FrameworkElement Build(Workspace ws, Action<string> setTheme, Action models, Action close, Action changed)
    {
        var col = new StackPanel { Spacing = 22, MaxWidth = 620, Padding = new Thickness(32, 28, 32, 32), HorizontalAlignment = HorizontalAlignment.Stretch };
        var head = new Grid();
        head.Children.Add(new TextBlock { Text = "Settings", FontFamily = Theme.SansMedium, FontSize = 17, Foreground = Theme.Ink });
        var x = new TextBlock { Text = "Esc", FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center };
        var xb = new Border { Child = x, Padding = new Thickness(6, 2, 6, 2), CornerRadius = new CornerRadius(6), HorizontalAlignment = HorizontalAlignment.Right, Background = new SolidColorBrush(Colors.Transparent) };
        xb.Tapped += (_, _) => close();
        head.Children.Add(xb);
        col.Children.Add(head);

        col.Children.Add(Row("Appearance", "Follow Windows, or pick one.",
            Segmented([("System", "system"), ("Light", "light"), ("Dark", "dark")], ws.ThemePref, v => { ws.ThemePref = v; ws.Save(); setTheme(v); })));
        col.Children.Add(Row("Default layout", "How a project opens: the chat alone, or the chat with code beside it.",
            Segmented([("Chat", "chat"), ("Split", "split")], ws.DefaultLayout, v => { ws.DefaultLayout = v; ws.Save(); changed(); })));
        var reopen = new ToggleSwitch { IsOn = ws.Reopen, OnContent = "", OffContent = "", MinWidth = 0 };
        reopen.Toggled += (_, _) => { ws.Reopen = reopen.IsOn; ws.Save(); };
        col.Children.Add(Row("Reopen last project", "Start where you left off.", reopen));
        var connect = Segmented([("Connect models…", "go")], "", _ => models());
        col.Children.Add(Row("Models", "API keys, ChatGPT sign-in, the default model.", connect));

        var sv = new ScrollViewer { Content = col, IsTabStop = true };
        sv.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Escape) { e.Handled = true; close(); } };
        sv.Loaded += (_, _) => sv.Focus(FocusState.Programmatic);
        return sv;
    }

    static FrameworkElement Row(string title, string about, FrameworkElement control)
    {
        var g = new Grid { ColumnSpacing = 24 };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new StackPanel
        {
            Spacing = 2,
            Children =
            {
                new TextBlock { Text = title, FontFamily = Theme.Sans, FontSize = 13.5, Foreground = Theme.Ink },
                new TextBlock { Text = about, FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Faint, TextWrapping = TextWrapping.Wrap },
            },
        });
        control.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(control, 1);
        g.Children.Add(control);
        return new Border { BorderBrush = Theme.Line, BorderThickness = new Thickness(0, 0, 0, 1), Padding = new Thickness(0, 0, 0, 16), Child = g };
    }

    /// the layout switch's look: a tile with the picked one raised as a card
    static FrameworkElement Segmented((string Label, string Tag)[] opts, string on, Action<string> pick)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, Padding = new Thickness(2) };
        foreach (var (label, tag) in opts)
        {
            var sel = tag == on;
            var b = new Border
            {
                CornerRadius = new CornerRadius(6), Padding = new Thickness(12, 4, 12, 4), Background = sel ? Theme.Card : new SolidColorBrush(Colors.Transparent),
                Child = new TextBlock { Text = label, FontFamily = Theme.Sans, FontSize = 12.5, Foreground = sel ? Theme.Ink : Theme.Soft },
            };
            var rest = b.Background;
            b.PointerEntered += (_, _) => { if (!sel) b.Background = Theme.Hover; };
            b.PointerExited += (_, _) => b.Background = rest;
            b.Tapped += (_, _) => pick(tag);
            row.Children.Add(b);
        }
        return new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(8), Child = row };
    }
}
