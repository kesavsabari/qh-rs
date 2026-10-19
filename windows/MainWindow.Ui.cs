using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.ApplicationModel.DataTransfer;
using Windows.Graphics.Imaging;
using Windows.Storage.Streams;

namespace Qh;

/// Settings, theme, the tab bar's ⋯ menu, the composer's model/mode buttons and image attachments.
sealed partial class MainWindow
{
    bool showSettings;
    readonly Border bottomHolder = new();
    readonly Border frame = new();
    Border? composerCard, paletteCard;
    FontIcon? attachIcon;
    readonly StackPanel metaButtons = new() { Orientation = Orientation.Horizontal, Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
    readonly StackPanel chips = new() { Orientation = Orientation.Horizontal, Spacing = 6, Padding = new Thickness(8, 4, 8, 0), Visibility = Visibility.Collapsed };
    readonly List<(string Name, string Data)> attached = [];

    // MARK: theme

    void ThemeFlags(string v)
    {
        Theme.Force = v == "dark" ? true : v == "light" ? false : null;
        root.RequestedTheme = v == "dark" ? ElementTheme.Dark : v == "light" ? ElementTheme.Light : ElementTheme.Default;
    }

    /// brushes are made from Theme when a view is drawn: set the flag, repaint what Build made once, redraw the rest
    void Retheme(string v)
    {
        ThemeFlags(v);
        root.Background = Theme.Ground;
        frame.Background = Theme.Card; frame.BorderBrush = Theme.Line;
        if (composerCard != null) { composerCard.Background = Theme.Card; composerCard.BorderBrush = Theme.Line; }
        if (paletteCard != null) { paletteCard.Background = Theme.Card; paletteCard.BorderBrush = Theme.Line; }
        suggestBox.BorderBrush = Theme.Line;
        if (send.Child is FontIcon si) si.Foreground = Theme.Card;
        if (attachIcon != null) attachIcon.Foreground = Theme.Soft;
        meta.Foreground = Theme.Faint;
        bottomHolder.Child = null;
        if (dots.Parent is Panel dp) dp.Children.Remove(dots);
        bottomHolder.Child = SidebarBottom();
        var c = captionsShown; captionsShown = !c; SetCaptions(c);
        threadView.Reset();
        DrawChips();
        Render();
    }

    // MARK: settings

    void ToggleSettings()
    {
        showSettings = !showSettings;
        if (!showSettings) input.Focus(FocusState.Programmatic);
        Render();
    }

    void OpenModels()
    {
        showSettings = false;
        if (ws.Current is { } p) { p.ShowSetup = true; p.Page = null; p.LoadSetup(); }
        Render();
    }

    // MARK: tab bar ⋯

    FrameworkElement MoreButton(Project p)
    {
        var b = Plain(Label("⋯", 15, Theme.Soft), () => { }, "More");
        var menu = new MenuFlyout { Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.BottomEdgeAlignedRight };
        void Item(string text, string? key, Action a)
        {
            var i = new MenuFlyoutItem { Text = text, FontFamily = Theme.Sans };
            if (key != null) i.KeyboardAcceleratorTextOverride = key;
            i.Click += (_, _) => a();
            menu.Items.Add(i);
        }
        Item("History · checkpoints", null, () => { showSettings = false; Page(p, "history"); });
        Item("Connect models", null, OpenModels);
        Item("Review changes", "Ctrl+D", () => { showSettings = false; Page(p, "review"); });
        b.Tapped += (_, _) => menu.ShowAt(b);
        return b;
    }

    // MARK: composer meta: model and mode as quiet buttons

    void RenderMetaButtons(Tab? t)
    {
        metaButtons.Children.Clear();
        if (t == null || ws.Current is not { } p) return;
        var model = Theme.ShortModel(t.Model, t.Provider);
        if (model.Length > 0) metaButtons.Children.Add(MetaButton(model, "Change model", () => p.Prompt("/model")));
        if (p.Mode.Length > 0)
            metaButtons.Children.Add(MetaButton(p.Mode, "Next mode", () =>
            {
                var i = Array.IndexOf(Project.Modes, p.Mode);
                p.SetMode(Project.Modes[(i + 1) % Project.Modes.Length]);
            }));
    }

    static Border MetaButton(string text, string tip, Action a)
    {
        var chev = new FontIcon { Glyph = "", FontSize = 8, Foreground = Theme.Faint, Opacity = 0, Margin = new Thickness(4, 1, 0, 0) };
        var row = new StackPanel { Orientation = Orientation.Horizontal, Children = { new TextBlock { Text = text, FontFamily = Theme.Sans, FontSize = 11.5, Foreground = Theme.Faint }, chev } };
        var clear = new SolidColorBrush(Colors.Transparent);
        var b = new Border { Child = row, Padding = new Thickness(6, 3, 6, 3), CornerRadius = new CornerRadius(6), Background = clear, VerticalAlignment = VerticalAlignment.Center };
        b.PointerEntered += (_, _) => { b.Background = Theme.Hover; chev.Opacity = 1; };
        b.PointerExited += (_, _) => { b.Background = clear; chev.Opacity = 0; };
        b.Tapped += (_, _) => a();
        ToolTipService.SetToolTip(b, tip);
        return b;
    }

    // MARK: images: picked or pasted, sent as base64 in the prompt's "images" (what qh's /image sends)

    async void PickImages()
    {
        var picker = new Windows.Storage.Pickers.FileOpenPicker();
        foreach (var ext in new[] { ".png", ".jpg", ".jpeg", ".gif", ".webp" }) picker.FileTypeFilter.Add(ext);
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var files = await picker.PickMultipleFilesAsync();
        if (files == null) return;
        foreach (var f in files)
        {
            try { attached.Add((f.Name, Convert.ToBase64String(await File.ReadAllBytesAsync(f.Path)))); } catch { }
        }
        DrawChips();
    }

    async void PasteImage(object sender, TextControlPasteEventArgs e)
    {
        var dp = Clipboard.GetContent();
        if (!dp.Contains(StandardDataFormats.Bitmap) || dp.Contains(StandardDataFormats.Text)) return;
        e.Handled = true;
        try
        {
            var r = await dp.GetBitmapAsync();
            using var src = await r.OpenReadAsync();
            var dec = await BitmapDecoder.CreateAsync(src);
            using var bmp = await dec.GetSoftwareBitmapAsync(BitmapPixelFormat.Bgra8, BitmapAlphaMode.Premultiplied);
            using var ms = new InMemoryRandomAccessStream();
            var enc = await BitmapEncoder.CreateAsync(BitmapEncoder.PngEncoderId, ms);
            enc.SetSoftwareBitmap(bmp);
            await enc.FlushAsync();
            ms.Seek(0);
            using var mem = new MemoryStream();
            await ms.AsStreamForRead().CopyToAsync(mem);
            var bytes = mem.ToArray();
            var path = Path.Combine(Path.GetTempPath(), $"qh-paste-{DateTime.Now:yyyyMMdd-HHmmss-fff}.png");
            await File.WriteAllBytesAsync(path, bytes);
            attached.Add((Path.GetFileName(path), Convert.ToBase64String(bytes)));
            DrawChips();
        }
        catch { }
    }

    void DrawChips()
    {
        chips.Children.Clear();
        chips.Visibility = attached.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        foreach (var a in attached.ToList())
        {
            var x = new TextBlock { Text = "×", FontFamily = Theme.Sans, FontSize = 13, Foreground = Theme.Faint, VerticalAlignment = VerticalAlignment.Center };
            var xb = new Border { Child = x, Padding = new Thickness(4, 0, 2, 0), Background = new SolidColorBrush(Colors.Transparent) };
            xb.Tapped += (_, _) => { attached.Remove(a); DrawChips(); };
            ToolTipService.SetToolTip(xb, "Remove");
            var row = new StackPanel
            {
                Orientation = Orientation.Horizontal, Spacing = 4,
                Children = { new FontIcon { Glyph = "", FontSize = 11, Foreground = Theme.Soft }, new TextBlock { Text = a.Name, FontFamily = Theme.Sans, FontSize = 12, Foreground = Theme.Soft, MaxWidth = 160, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center }, xb },
            };
            chips.Children.Add(new Border { Background = Theme.Tile, CornerRadius = new CornerRadius(7), Padding = new Thickness(8, 4, 6, 4), Child = row });
        }
        RenderMeta();
    }
}
