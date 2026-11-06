using System.Numerics;
using Microsoft.UI.Composition;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Hosting;
using Microsoft.UI.Xaml.Markup;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using uniffi.trs_ffi;
using Windows.UI;

namespace Seedwell;

/// <summary>
/// The proxy's icon, a shield with the proxy logo, the Mac's vectors and colours (docs/proxy-shield.md): a 24-unit box,
/// y down, drawn in five looks from the core's <c>ProxyIcon</c>. In a contrast theme it takes the theme's colours and
/// the state reads from outline or solid. Its glow (a Composition shadow in the shield's own colour) is
/// <see cref="ProxyShieldButton"/>'s hover.
/// </summary>
sealed class ProxyShield : Grid
{
    const string ShieldData = "M 10.6,2.7 C 11.5,2.35 12.5,2.35 13.4,2.7 L 19.3,4.55 C 20.2,4.85 20.6,5.2 20.6,5.9 L 20.6,11 C 20.6,15.9 17,19.3 13.1,21.25 C 12.4,21.65 11.6,21.65 10.9,21.25 C 7,19.3 3.4,15.9 3.4,11 L 3.4,5.9 C 3.4,5.2 3.8,4.85 4.7,4.55 Z";
    const string LogoData = "M 6.7,11 L 8.95,11 M 12.85,11 L 17.1,11 M 14.8,8.7 L 17.1,11 L 14.8,13.3";

    readonly double size;
    readonly Canvas glowHost = new();
    readonly Microsoft.UI.Xaml.Shapes.Path shield = Stroked(2, (Geometry)XamlBindingHelper.ConvertValue(typeof(Geometry), ShieldData));
    readonly Microsoft.UI.Xaml.Shapes.Path logo = Stroked(1.9, (Geometry)XamlBindingHelper.ConvertValue(typeof(Geometry), LogoData));
    readonly Microsoft.UI.Xaml.Shapes.Path ring = Stroked(1.9, new EllipseGeometry { Center = new Windows.Foundation.Point(10.9, 11), RadiusX = 1.95, RadiusY = 1.95 });
    DropShadow? shadow;
    ProxyIcon icon = ProxyIcon.NoKey;

    internal ProxyShield(double size)
    {
        this.size = size;
        Width = size;
        Height = size;
        var box = new Canvas { Width = 24, Height = 24 };
        box.Children.Add(shield);
        box.Children.Add(logo);
        box.Children.Add(ring);
        Children.Add(glowHost);
        Children.Add(new Viewbox { Width = size, Height = size, Child = box });
        ActualThemeChanged += (_, _) => Paint();
        Paint();
    }

    static Microsoft.UI.Xaml.Shapes.Path Stroked(double width, Geometry data) => new()
    {
        Data = data,
        // A size of its own, for the alpha mask the glow is made from.
        Width = 24,
        Height = 24,
        StrokeThickness = width,
        StrokeLineJoin = PenLineJoin.Round,
        StrokeStartLineCap = PenLineCap.Round,
        StrokeEndLineCap = PenLineCap.Round,
    };

    internal ProxyIcon Icon
    {
        get => icon;
        set
        {
            if (icon != value)
            {
                icon = value;
                Paint();
            }
        }
    }

    static bool Contrast => new Windows.UI.ViewManagement.AccessibilitySettings().HighContrast;

    static bool Solid(ProxyIcon i) => i is ProxyIcon.On or ProxyIcon.Slow or ProxyIcon.Failed;

    /// <summary>The shield's and the logo's colours (docs/proxy-shield.md, Colours).</summary>
    internal static (Color Shield, Color Logo) Colours(ProxyIcon i, bool dark) => i switch
    {
        ProxyIcon.NoKey => dark ? (Rgb(0x98989D), Rgb(0x98989D)) : (Rgb(0x8E8E93), Rgb(0x8E8E93)),
        ProxyIcon.Off => dark ? (Rgb(0x30D158), Rgb(0xC7C7CC)) : (Rgb(0x34C759), Rgb(0x48484A)),
        ProxyIcon.On => (dark ? Rgb(0x30D158) : Rgb(0x34C759), Rgb(0xFFFFFF)),
        ProxyIcon.Slow => (dark ? Rgb(0xFF9230) : Rgb(0xFF8D28), Rgb(0xFFFFFF)),
        _ => (dark ? Rgb(0xFF4245) : Rgb(0xFF383C), Rgb(0xFFFFFF)),
    };

    static Color Rgb(uint v) => Color.FromArgb(255, (byte)(v >> 16), (byte)(v >> 8), (byte)v);

    static Color Theme(string key) => (Color)Application.Current.Resources[key];

    Color ShieldColour => Colours(icon, ActualTheme == ElementTheme.Dark).Shield;

    void Paint()
    {
        Color outer, inner;
        if (Contrast)
        {
            outer = Solid(icon) ? Theme("SystemColorHighlightColor") : Theme("SystemColorWindowTextColor");
            inner = Solid(icon) ? Theme("SystemColorHighlightTextColor") : Theme("SystemColorWindowTextColor");
        }
        else
        {
            (outer, inner) = Colours(icon, ActualTheme == ElementTheme.Dark);
        }
        shield.Stroke = new SolidColorBrush(outer);
        shield.Fill = Solid(icon) ? new SolidColorBrush(outer) : null;
        logo.Stroke = new SolidColorBrush(inner);
        ring.Stroke = new SolidColorBrush(inner);
    }

    /// <summary>The glow to this opacity over 0.15 s: blur 3.5 at 16, scaled with the size, no offset. None in a contrast theme.</summary>
    internal void Glow(float opacity)
    {
        if (Contrast)
        {
            return;
        }
        var compositor = ElementCompositionPreview.GetElementVisual(this).Compositor;
        if (shadow is null)
        {
            shadow = compositor.CreateDropShadow();
            shadow.BlurRadius = (float)(3.5 * size / 16);
            shadow.Offset = Vector3.Zero;
            shadow.Mask = shield.GetAlphaMask();
            shadow.Opacity = 0;
            var sprite = compositor.CreateSpriteVisual();
            sprite.Size = new Vector2((float)size);
            sprite.Shadow = shadow;
            ElementCompositionPreview.SetElementChildVisual(glowHost, sprite);
        }
        shadow.Color = ShieldColour;
        var fade = compositor.CreateScalarKeyFrameAnimation();
        fade.InsertKeyFrame(1, opacity);
        fade.Duration = TimeSpan.FromMilliseconds(150);
        shadow.StartAnimation(nameof(DropShadow.Opacity), fade);
        GlowTarget = opacity;
    }

    /// <summary>The opacity the glow last went to (QA: a render does not capture Composition's shadow).</summary>
    internal float GlowTarget { get; private set; }
}

/// <summary>
/// The shield where it can be clicked (the tray flyout's footer, Settings › Network, the SOCKS5 window's header): no
/// chrome in any state, the same hit target, the glow on hover. Click and right click are <see cref="ProxyUi"/>'s.
/// </summary>
sealed class ProxyShieldButton : Button
{
    static ControlTemplate? plain;
    readonly ProxyUi ui;
    readonly ProxyShield shield;
    bool hovered;

    internal ProxyShieldButton(ProxyUi ui, double size)
    {
        this.ui = ui;
        shield = new ProxyShield(size);
        Content = shield;
        Padding = new Thickness(0);
        VerticalAlignment = VerticalAlignment.Center;
        // Focus and keys as any button's; nothing drawn but the content.
        Template = plain ??= (ControlTemplate)XamlReader.Load(
            "<ControlTemplate xmlns='http://schemas.microsoft.com/winfx/2006/xaml/presentation' TargetType='Button'>" +
            "<ContentPresenter Background='Transparent' Content='{TemplateBinding Content}' /></ControlTemplate>");
        Click += (_, _) => ui.Click();
        RightTapped += (_, e) =>
        {
            ui.ShowMenu(this);
            e.Handled = true;
        };
        // (A button takes the pointer's press for itself: handled ones too.)
        AddHandler(PointerEnteredEvent, new Microsoft.UI.Xaml.Input.PointerEventHandler((_, _) => Hover(true)), true);
        AddHandler(PointerExitedEvent, new Microsoft.UI.Xaml.Input.PointerEventHandler((_, _) => Hover(false)), true);
        AddHandler(PointerPressedEvent, new Microsoft.UI.Xaml.Input.PointerEventHandler((_, _) => shield.Glow(1)), true);
        AddHandler(PointerReleasedEvent, new Microsoft.UI.Xaml.Input.PointerEventHandler((_, _) => shield.Glow(hovered ? 0.75f : 0)), true);
        Loaded += (_, _) =>
        {
            ui.Changed += Follow;
            Follow();
        };
        Unloaded += (_, _) => ui.Changed -= Follow;
        Follow();
    }

    internal void Hover(bool on)
    {
        hovered = on;
        shield.Glow(on ? 0.75f : 0);
    }

    /// <summary>The model's look and words; no click while a call runs (the core would refuse it).</summary>
    void Follow()
    {
        var m = ui.Model;
        shield.Icon = m.Icon;
        IsEnabled = !ui.Busy;
        ToolTipService.SetToolTip(this, m.Tooltip);
        AutomationProperties.SetName(this, m.Tooltip);
    }

    internal string QaDescribe() => $"{shield.Icon} enabled {IsEnabled} glow {shield.GlowTarget}";
}
