using System.Drawing;
using System.Drawing.Drawing2D;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The notification-area icon, drawn from the core's <c>IconLook</c> the way the Mac draws its menu bar icon
/// (app/Sources/SeedwellApp/Menu/Drawing.swift): a down and an up arrow, grey when idle and green (download) or red
/// (upload) with the rate; a faint hairline with the total download progress, or a green bar while files are moved;
/// a small orange dot when something waits for the user. Drawn on the Mac's 20 x 18 grid, scaled to the tray's size.
/// </summary>
static class TrayIcon
{
    static readonly Color Green = Color.FromArgb(52, 199, 89);
    static readonly Color Red = Color.FromArgb(255, 59, 48);
    static readonly Color Orange = Color.FromArgb(255, 149, 0);

    /// <summary>The tray's icon size in pixels for the system DPI (16 at 100 %, 20, 24, 32 at 200 %).</summary>
    public static int Size()
    {
        uint dpi = GetDpiForSystem();
        return dpi >= 192 ? 32 : dpi >= 144 ? 24 : dpi >= 120 ? 20 : 16;
    }

    /// <summary>Whether the taskbar is light (black glyphs) or dark (white glyphs, the default).</summary>
    static bool LightTaskbar()
    {
        using var k = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        return k?.GetValue("SystemUsesLightTheme") is int v && v != 0;
    }

    public static Icon Draw(IconLook look, int px)
    {
        using var bmp = new Bitmap(px, px, System.Drawing.Imaging.PixelFormat.Format32bppArgb);
        using (var g = Graphics.FromImage(bmp))
        {
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.Clear(Color.Transparent);
            float s = px / 20f;
            float top = (px - 18 * s) / 2;
            // The Mac's grid has y going up from the bottom; GDI+ goes down from the top.
            PointF P(float x, float y) => new(x * s, top + (18 - y) * s);
            var label = LightTaskbar() ? Color.Black : Color.White;
            var grey = Color.FromArgb(128, label);
            Color Tint(Color c, double t)
            {
                if (t <= 0)
                {
                    return grey;
                }
                // Even the faintest step is visibly tinted; full speed is the plain colour.
                double f = 0.3 + 0.7 * t;
                int Mix(int a, int b) => (int)Math.Round(a + (b - a) * f);
                return Color.FromArgb(Mix(grey.A, 255), Mix(grey.R, c.R), Mix(grey.G, c.G), Mix(grey.B, c.B));
            }
            void Arrow(float x, bool up, Color c)
            {
                var (a, b) = up ? (4f, 15f) : (15f, 4f);
                float d = up ? -3.2f : 3.2f;
                // Relatively heavier below 32 px, so that the arrows still read at 16 px.
                using var pen = new Pen(c, px < 32 ? Math.Max(1.6f, 1.9f * s) : 1.6f * s) { StartCap = LineCap.Round, EndCap = LineCap.Round, LineJoin = LineJoin.Round };
                g.DrawLine(pen, P(x, a), P(x, b));
                g.DrawLines(pen, [P(x - 3.4f, b + d), P(x, b), P(x + 3.4f, b + d)]);
            }
            Arrow(6.5f, false, Tint(Green, look.Down));
            Arrow(13.5f, true, Tint(Red, look.Up));
            void Bar(Color track, Color fill, double fraction)
            {
                float h = Math.Max(1f, s);
                var at = P(3, 2);
                using (var t = new SolidBrush(track))
                {
                    g.FillRectangle(t, at.X, at.Y, 14 * s, h);
                }
                using var f = new SolidBrush(fill);
                g.FillRectangle(f, at.X, at.Y, 14 * s * (float)Math.Clamp(fraction, 0.05, 1), h);
            }
            if (look.Moving is double moving)
            {
                Bar(Color.FromArgb(71, Green), Green, moving);
            }
            else if (look.Progress is double progress)
            {
                Bar(Color.FromArgb(46, label), Color.FromArgb(140, label), progress);
            }
            if (look.Attention)
            {
                var c = P(18, 2);
                float r = Math.Max(1.5f, 1.5f * s);
                // A ring of clear pixels ends the progress hairline short of the dot.
                g.CompositingMode = CompositingMode.SourceCopy;
                using (var clear = new SolidBrush(Color.Transparent))
                {
                    g.FillEllipse(clear, c.X - r - s, c.Y - r - s, 2 * (r + s), 2 * (r + s));
                }
                g.CompositingMode = CompositingMode.SourceOver;
                using var dot = new SolidBrush(Color.FromArgb(191, Orange));
                g.FillEllipse(dot, c.X - r, c.Y - r, 2 * r, 2 * r);
            }
        }
        var h = bmp.GetHicon();
        var icon = (Icon)Icon.FromHandle(h).Clone();
        DestroyIcon(h);
        return icon;
    }

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern uint GetDpiForSystem();

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern bool DestroyIcon(IntPtr h);
}
