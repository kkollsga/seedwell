using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// What a message's text does and how it looks: <see cref="Open"/> is what a click on a link does (the web and mail with
/// the system, a seedwell:// link through <c>route_link</c>), <see cref="Image"/> what a click on an image reference does
/// (its gallery shows that image), and on an accent bubble the text is white and links underlined.
/// </summary>
internal sealed record MarkdownInk(Action<ChatLink> Open, Action<uint>? Image = null, bool OnAccent = false);

/// <summary>
/// A message's Markdown on Windows (the Mac's ChatMarkdown): the core's blocks and runs (<c>chat_blocks</c>, trs-ffi
/// chat_markdown.rs) drawn as text elements in the type scale (<see cref="Theme"/>). Nothing is parsed here; a link is
/// only what the core marks as one, and an image reference only a run with its <c>image</c> index.
/// </summary>
static class ChatMarkdown
{
    static Brush Res(string key) => Theme.Res(key);

    static readonly Brush White = new SolidColorBrush(Microsoft.UI.Colors.White);

    /// <summary>The blocks, 6 points apart.</summary>
    internal static StackPanel Blocks(IEnumerable<ChatBlock> blocks, MarkdownInk ink)
    {
        var s = new StackPanel { Spacing = 6 };
        foreach (var b in blocks)
        {
            s.Children.Add(Block(b, ink));
        }
        return s;
    }

    static UIElement Block(ChatBlock b, MarkdownInk ink)
    {
        switch (b)
        {
            case ChatBlock.Heading h:
                // Heading for h1, headline for h2 and h3 and the rest (the Mac's Theme).
                var heading = Text(h.Runs, ink, h.Level == 1 ? Theme.Heading : Theme.Headline, bold: true);
                heading.Margin = new Thickness(0, 4, 0, 0);
                return heading;
            case ChatBlock.Paragraph p:
                return Text(p.Runs, ink);
            case ChatBlock.CodeBlock c:
                return Code(c, ink);
            case ChatBlock.Item it:
                var item = new Grid { ColumnSpacing = 6, Margin = new Thickness(18 * Math.Min((int)it.Depth, 3), 0, 0, 0) };
                item.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(it.Numbered || it.Marker is "☐" or "☑" ? 18 : 10) });
                item.ColumnDefinitions.Add(new ColumnDefinition());
                var markerInk = ink.OnAccent ? White : Res("TextFillColorSecondaryBrush");
                // A task's box ("☐", "☑") as a quiet checkbox glyph at the text's size; other markers as text.
                item.Children.Add(it.Marker is "☐" or "☑"
                    ? new FontIcon { Glyph = it.Marker == "☑" ? "" : "", FontSize = Theme.Body - 1, Foreground = markerInk, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(0, 3, 0, 0) }
                    : new TextBlock { Text = it.Marker, FontSize = Theme.Body, LineHeight = Theme.BodyLine, HorizontalAlignment = HorizontalAlignment.Right, Foreground = markerInk });
                var itemText = Text(it.Runs, ink);
                Grid.SetColumn(itemText, 1);
                item.Children.Add(itemText);
                return item;
            case ChatBlock.Quote q:
                var quote = Text(q.Runs, ink);
                if (!ink.OnAccent) quote.Foreground = Res("TextFillColorSecondaryBrush");
                return new Border { BorderBrush = ink.OnAccent ? White : Res("TextFillColorTertiaryBrush"), BorderThickness = new Thickness(3, 0, 0, 0), Padding = new Thickness(8, 0, 0, 0), Child = quote };
            case ChatBlock.Rule:
                return new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush"), Margin = new Thickness(0, 2, 0, 2) };
            case ChatBlock.Table t:
                return Table(t, ink);
            default:
                return new TextBlock();
        }
    }

    /// <summary>
    /// Fenced code: monospaced, never wrapped (it scrolls sideways), its language word above it in the block, and a copy
    /// button in the top-right corner while the pointer is on it or it has keyboard focus (a check mark for 1.5 s after).
    /// </summary>
    static UIElement Code(ChatBlock.CodeBlock c, MarkdownInk ink)
    {
        var s = new StackPanel { Spacing = 2 };
        if (c.Language.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = c.Language, FontSize = Theme.Micro, FontWeight = FontWeights.SemiBold, Foreground = Res("TextFillColorSecondaryBrush") });
        }
        s.Children.Add(new ScrollViewer
        {
            HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
            VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
            HorizontalScrollMode = ScrollMode.Auto,
            Content = new TextBlock { Text = c.Text, FontFamily = Theme.MonoFont, FontSize = Theme.Mono, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.NoWrap },
        });
        var help = TrsFfiMethods.ChatCodeCopyHelp();
        var glyph = new FontIcon { Glyph = "", FontSize = 13 };
        var copy = new Button
        {
            Content = glyph,
            Padding = new Thickness(6),
            Opacity = 0,
            HorizontalAlignment = HorizontalAlignment.Right,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(0, 4, 4, 0),
        };
        ToolTipService.SetToolTip(copy, help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(copy, help);
        copy.Click += (_, _) =>
        {
            AppLog.Write("chat code: copied");
            // QA never touches the user's clipboard.
            if (!Quiet.On)
            {
                var package = new Windows.ApplicationModel.DataTransfer.DataPackage();
                package.SetText(c.Text);
                Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
            }
            glyph.Glyph = "";
            var timer = copy.DispatcherQueue.CreateTimer();
            timer.Interval = TimeSpan.FromSeconds(1.5);
            timer.IsRepeating = false;
            timer.Tick += (_, _) => glyph.Glyph = "";
            timer.Start();
        };
        copy.GotFocus += (_, _) => copy.Opacity = 1;
        copy.LostFocus += (_, _) => copy.Opacity = 0;
        var block = new Grid
        {
            Background = ink.OnAccent ? new SolidColorBrush(Microsoft.UI.Colors.White) { Opacity = 0.16 } : Res("SubtleFillColorSecondaryBrush"),
            CornerRadius = new CornerRadius(Theme.CodeRadius),
        };
        block.Children.Add(new Border { Padding = new Thickness(12, 8, 12, 8), Child = s });
        block.Children.Add(copy);
        block.PointerEntered += (_, _) => copy.Opacity = 1;
        block.PointerExited += (_, _) => { if (copy.FocusState == FocusState.Unfocused) copy.Opacity = 0; };
        return block;
    }

    static UIElement Table(ChatBlock.Table t, MarkdownInk ink)
    {
        var g = new Grid { ColumnSpacing = 14, RowSpacing = 4 };
        for (int c = 0; c < t.Header.Length; c++)
        {
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        }
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        for (int c = 0; c < t.Header.Length; c++)
        {
            var cell = Text(t.Header[c].Runs, ink, Theme.Secondary, bold: true);
            Grid.SetColumn(cell, c);
            g.Children.Add(cell);
        }
        var line = new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush") };
        Grid.SetRow(line, 1);
        Grid.SetColumnSpan(line, Math.Max(1, t.Header.Length));
        g.Children.Add(line);
        for (int r = 0; r < t.Rows.Length; r++)
        {
            g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            for (int c = 0; c < t.Rows[r].Cells.Length && c < t.Header.Length; c++)
            {
                var cell = Text(t.Rows[r].Cells[c].Runs, ink, Theme.Secondary);
                Grid.SetRow(cell, r + 2);
                Grid.SetColumn(cell, c);
                g.Children.Add(cell);
            }
        }
        return new ScrollViewer { HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, HorizontalScrollMode = ScrollMode.Auto, Content = g };
    }

    /// <summary>QA: a block's text as drawn (its runs joined; a code block's text; a rule as "---").</summary>
    internal static string Plain(ChatBlock b) => b switch
    {
        ChatBlock.Heading h => string.Concat(h.Runs.Select(r => r.Text)),
        ChatBlock.Paragraph p => string.Concat(p.Runs.Select(r => r.Text)),
        ChatBlock.CodeBlock c => c.Text,
        ChatBlock.Item i => $"{i.Marker} {string.Concat(i.Runs.Select(r => r.Text))}",
        ChatBlock.Quote q => "> " + string.Concat(q.Runs.Select(r => r.Text)),
        ChatBlock.Rule => "---",
        ChatBlock.Table t => string.Join(" | ", t.Header.Select(c => string.Concat(c.Runs.Select(r => r.Text)))),
        _ => "",
    };

    /// <summary>
    /// A block's runs as one text element (selectable; links clickable). An image reference is a chip where it stands: a
    /// photo glyph and its text in the accent colour (white and underlined on an accent bubble), which shows that image.
    /// </summary>
    internal static RichTextBlock Text(IEnumerable<ChatRun> runs, MarkdownInk ink, double size = Theme.Body, bool bold = false)
    {
        var p = new Paragraph();
        foreach (var r in runs)
        {
            var run = new Run { Text = r.Text };
            if (r.Code)
            {
                run.FontFamily = Theme.MonoFont;
                run.FontSize = size - 1;
            }
            if (r.Bold || bold) run.FontWeight = FontWeights.SemiBold;
            if (r.Italic) run.FontStyle = Windows.UI.Text.FontStyle.Italic;
            if (r.Strikethrough) run.TextDecorations = Windows.UI.Text.TextDecorations.Strikethrough;
            if (r.Image is uint image && ink.Image is Action<uint> show)
            {
                var chip = new Hyperlink { UnderlineStyle = ink.OnAccent ? UnderlineStyle.Single : UnderlineStyle.None };
                if (ink.OnAccent) chip.Foreground = White;
                chip.Inlines.Add(new Run { Text = " ", FontFamily = new FontFamily("Segoe Fluent Icons"), FontSize = size - 2 });
                chip.Inlines.Add(run);
                chip.Click += (_, _) => show(image);
                p.Inlines.Add(chip);
            }
            else if (r.Link is ChatLink link)
            {
                var h = new Hyperlink();
                if (ink.OnAccent) { h.Foreground = White; h.UnderlineStyle = UnderlineStyle.Single; }
                h.Inlines.Add(run);
                ToolTipService.SetToolTip(h, link.Url);
                h.Click += (_, _) => ink.Open(link);
                p.Inlines.Add(h);
            }
            else
            {
                p.Inlines.Add(run);
            }
        }
        var t = new RichTextBlock { FontSize = size, LineHeight = size >= Theme.Body ? Theme.BodyLine * size / Theme.Body : Theme.SecondaryLine, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.Wrap };
        if (ink.OnAccent) t.Foreground = White;
        t.Blocks.Add(p);
        return t;
    }
}
