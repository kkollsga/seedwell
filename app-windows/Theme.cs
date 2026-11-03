using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace Seedwell;

/// <summary>
/// One type and spacing scale for the app (the Mac's Support/Theme.swift, build 49), mapped to Segoe UI Variable and the
/// Fluent ramp: the Mac's 13 pt body is 14 epx here. Sizes in epx; spacing on a 4 grid (2, 4, 8, 12, 16, 24, 32).
/// </summary>
static class Theme
{
    internal const double Title = 20, Heading = 16, Headline = 14, Body = 14, Secondary = 13, Caption = 12, Micro = 11;
    internal const double Mono = 13, MonoSmall = 12;
    /// <summary>Line heights: body text +6 (14 on 20, the Mac's 13 on ~18), secondary +5.</summary>
    internal const double BodyLine = 20, SecondaryLine = 18;
    internal static readonly FontFamily MonoFont = new("Cascadia Mono, Consolas");
    /// <summary>Radii: chips and run blocks, code and file cards, the composer, images, bubbles.</summary>
    internal const double ChipRadius = 6, CodeRadius = 8, ComposerRadius = 12, ImageRadius = 10, BubbleRadius = 16;
    /// <summary>Above a run's first message, and between the messages of a run (the user's choice: they touch).</summary>
    internal const double RunGap = 16, InRunGap = 0;

    internal static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    internal static Color Color(string key) => ((SolidColorBrush)Res(key)).Color;

    internal static SolidColorBrush Brush(Color c, double opacity = 1) => new(c) { Opacity = opacity };

    internal static Color Accent => Color("AccentFillColorDefaultBrush");

    static readonly Color Grey = Windows.UI.Color.FromArgb(255, 142, 142, 147);

    /// <summary>The muted accent of the user's agents: the accent blended about 65 % with grey.</summary>
    internal static Color MutedAccent => Mix(Accent, Grey, 0.65);

    /// <summary>A faint fill in the text colour (4.5 % hover, 5 % quotes, 8 % bubbles, 10 % strokes).</summary>
    internal static SolidColorBrush Faint(double opacity) => Brush(Color("TextFillColorPrimaryBrush"), opacity);

    // The Mac's palette for other people's edges, by the core's index (chat_palette_size): HSB hues in degrees; saturation
    // 0.42 light, 0.36 dark; brightness 0.68 light, 0.78 dark; their agents' at 0.45 of the saturation.
    static readonly double[] Hues = [28, 44, 62, 168, 262, 288, 318, 342];

    internal static Color Person(uint index, bool agent, bool dark)
    {
        var s = (dark ? 0.36 : 0.42) * (agent ? 0.45 : 1);
        return Hsb(Hues[index % (uint)Hues.Length], s, dark ? 0.78 : 0.68);
    }

    static Color Mix(Color a, Color b, double t) => Windows.UI.Color.FromArgb(255,
        (byte)Math.Round(a.R + (b.R - a.R) * t), (byte)Math.Round(a.G + (b.G - a.G) * t), (byte)Math.Round(a.B + (b.B - a.B) * t));

    static Color Hsb(double h, double s, double v)
    {
        var c = v * s;
        var x = c * (1 - Math.Abs(h / 60 % 2 - 1));
        var (r, g, b) = (h / 60) switch
        {
            < 1 => (c, x, 0.0),
            < 2 => (x, c, 0.0),
            < 3 => (0.0, c, x),
            < 4 => (0.0, x, c),
            < 5 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        var m = v - c;
        return Windows.UI.Color.FromArgb(255, (byte)Math.Round((r + m) * 255), (byte)Math.Round((g + m) * 255), (byte)Math.Round((b + m) * 255));
    }
}
