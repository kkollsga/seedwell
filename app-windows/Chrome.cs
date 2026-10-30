using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Seedwell;

/// <summary>
/// A window's title bar in the app's own look: the content (and its Mica) runs up under the title bar, the caption
/// buttons sit on it with no background of their own and follow the light or dark theme, and a thin strip with the
/// icon and the title is the part the user drags. Without it a dark window keeps the system's white title bar.
/// </summary>
static class Chrome
{
    public const double Height = 32;

    /// <summary>Puts <paramref name="content"/> under a custom title strip in <paramref name="window"/>.</summary>
    public static void Apply(Window window, UIElement content, string title)
    {
        window.ExtendsContentIntoTitleBar = true;
        var bar = window.AppWindow.TitleBar;
        bar.ButtonBackgroundColor = Colors.Transparent;
        bar.ButtonInactiveBackgroundColor = Colors.Transparent;
        bar.PreferredTheme = TitleBarTheme.UseDefaultAppMode;

        var strip = new Grid { Height = Height, Padding = new Thickness(12, 0, 140, 0), ColumnSpacing = 10 };
        strip.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        strip.ColumnDefinitions.Add(new ColumnDefinition());
        strip.Children.Add(new Image
        {
            Source = new Microsoft.UI.Xaml.Media.Imaging.BitmapImage(new Uri(Path.Combine(AppContext.BaseDirectory, "Seedwell.ico"))),
            Width = 16,
            Height = 16,
            VerticalAlignment = VerticalAlignment.Center,
        });
        var text = new TextBlock { Text = title, FontSize = 12, VerticalAlignment = VerticalAlignment.Center, Opacity = 0.8 };
        Grid.SetColumn(text, 1);
        strip.Children.Add(text);

        var root = new Grid();
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition());
        root.Children.Add(strip);
        Grid.SetRow((FrameworkElement)content, 1);
        root.Children.Add(content);
        window.Content = root;
        window.SetTitleBar(strip);
    }
}
