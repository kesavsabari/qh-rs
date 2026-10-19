using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace Qh;

/// The qh design, same values as apple/qh/Theme.swift: paper and ink, one signal orange, Geist.
/// Light and dark follow Windows.
static class Theme
{
    public static bool Dark => Application.Current.RequestedTheme == ApplicationTheme.Dark;

    static SolidColorBrush Dyn(uint light, uint dark) => new(Hex(Dark ? dark : light));
    public static Color Hex(uint h) => Color.FromArgb(0xFF, (byte)(h >> 16), (byte)(h >> 8), (byte)h);

    public static SolidColorBrush Ink => Dyn(0x0E0E0D, 0xECEBE6);
    public static SolidColorBrush Soft => Dyn(0x6B6B66, 0x9C9B94);
    public static SolidColorBrush Faint => Dyn(0x9A9A94, 0x6F6E68);
    public static SolidColorBrush Hover => Dyn(0xE6E6E0, 0x1C1C1A);
    public static SolidColorBrush Accent => Dyn(0xFF4D1C, 0xFF5A2A);
    // chrome sits on a warm ground, the content floats on it as a card
    public static SolidColorBrush Ground => Dyn(0xECEBE6, 0x141413);
    public static SolidColorBrush Card => Dyn(0xFBFBF9, 0x1D1D1B);
    public static SolidColorBrush Tile => Dyn(0xF0EFEA, 0x262624);
    public static SolidColorBrush Line => Dyn(0xE3E2DC, 0x2A2A28);

    static string Font(string file, string family) => $"{AppContext.BaseDirectory}Fonts\\{file}#{family}";
    public static FontFamily Sans => new(Font("Geist-Regular.ttf", "Geist"));
    public static FontFamily SansMedium => new(Font("Geist-Medium.ttf", "Geist Medium"));
    public static FontFamily Mono => new(Font("GeistMono-Regular.ttf", "Geist Mono"));

    /// "openrouter:anthropic/claude-opus-5.5" → "claude-opus-5.5", "ollama:qwen3.5:9b" → "qwen3.5:9b":
    /// the provider is noise in the composer, the model's own tag isn't
    public static string ShortModel(string m)
    {
        var i = m.IndexOf(':');
        var rest = i >= 0 ? m[(i + 1)..] : m;
        return rest[(rest.LastIndexOf('/') + 1)..] is { Length: > 0 } s ? s : m;
    }
}
