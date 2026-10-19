using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Qh;

/// A new tab: a quiet greeting, a few starter prompts (click fills the composer), the shortcuts.
static class EmptyChat
{
    static readonly string[] Starters =
    [
        "Explain how this project is organized",
        "Find a bug and fix it",
        "Write tests for the code I touched last",
        "What would you clean up first?",
    ];

    public static FrameworkElement Build(string project, Action<string> fill)
    {
        var col = new StackPanel { Spacing = 18, MaxWidth = 560, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Padding = new Thickness(32, 0, 32, 40) };
        col.Children.Add(new TextBlock
        {
            Text = $"What are we doing in {project}?", FontFamily = Theme.SansMedium, FontSize = 20, Foreground = Theme.Ink,
            HorizontalAlignment = HorizontalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis,
        });
        var grid = new Grid { ColumnSpacing = 8, RowSpacing = 8 };
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        grid.RowDefinitions.Add(new RowDefinition());
        grid.RowDefinitions.Add(new RowDefinition());
        for (int i = 0; i < Starters.Length; i++)
        {
            var text = Starters[i];
            var tile = new Border
            {
                Background = Theme.Tile, CornerRadius = new CornerRadius(10), Padding = new Thickness(14, 12, 14, 12),
                Child = new TextBlock { Text = text, FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Soft, TextWrapping = TextWrapping.Wrap },
            };
            var rest = Theme.Tile;
            tile.PointerEntered += (_, _) => tile.Background = Theme.Hover;
            tile.PointerExited += (_, _) => tile.Background = rest;
            tile.Tapped += (_, _) => fill(text);
            Grid.SetRow(tile, i / 2);
            Grid.SetColumn(tile, i % 2);
            grid.Children.Add(tile);
        }
        col.Children.Add(grid);
        col.Children.Add(new TextBlock
        {
            Text = "Ctrl+K palette  ·  / commands  ·  @ files", FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint,
            HorizontalAlignment = HorizontalAlignment.Center,
        });
        return col;
    }
}
