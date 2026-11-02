using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// A message's Markdown on Windows (the Mac's ChatMarkdown): the core's blocks and runs (<c>chat_blocks</c>, trs-ffi
/// chat_markdown.rs) drawn as text elements. Nothing is parsed here; a link is only what the core marks as one, and
/// <paramref name="open"/> is what a click on it does (the web and mail with the system, a seedwell:// link through
/// <c>route_link</c>).
/// </summary>
static class ChatMarkdown
{
    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static readonly FontFamily Mono = new("Consolas");

    /// <summary>The blocks, 6 points apart.</summary>
    internal static StackPanel Blocks(IEnumerable<ChatBlock> blocks, Action<ChatLink> open)
    {
        var s = new StackPanel { Spacing = 6 };
        foreach (var b in blocks)
        {
            s.Children.Add(Block(b, open));
        }
        return s;
    }

    static UIElement Block(ChatBlock b, Action<ChatLink> open)
    {
        switch (b)
        {
            case ChatBlock.Heading h:
                var heading = Text(h.Runs, open, h.Level switch { 1 => 16, 2 => 14.5, _ => 13.5 }, bold: true);
                heading.Margin = new Thickness(0, 4, 0, 0);
                return heading;
            case ChatBlock.Paragraph p:
                return Text(p.Runs, open);
            case ChatBlock.CodeBlock c:
                var code = new StackPanel { Spacing = 2 };
                if (c.Language.Length > 0)
                {
                    code.Children.Add(new TextBlock { Text = c.Language, FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush") });
                }
                code.Children.Add(new Border
                {
                    Background = Res("SubtleFillColorSecondaryBrush"),
                    CornerRadius = new CornerRadius(6),
                    Padding = new Thickness(8, 6, 8, 6),
                    Child = new ScrollViewer
                    {
                        HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
                        VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
                        HorizontalScrollMode = ScrollMode.Auto,
                        Content = new TextBlock { Text = c.Text, FontFamily = Mono, FontSize = 12, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.NoWrap },
                    },
                });
                return code;
            case ChatBlock.Item it:
                var item = new Grid { ColumnSpacing = 6, Margin = new Thickness(18 * Math.Min((int)it.Depth, 3), 0, 0, 0) };
                item.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(it.Numbered ? 18 : 10) });
                item.ColumnDefinitions.Add(new ColumnDefinition());
                item.Children.Add(new TextBlock { Text = it.Marker, HorizontalAlignment = HorizontalAlignment.Right, Foreground = Res("TextFillColorSecondaryBrush") });
                var itemText = Text(it.Runs, open);
                Grid.SetColumn(itemText, 1);
                item.Children.Add(itemText);
                return item;
            case ChatBlock.Quote q:
                var quote = Text(q.Runs, open);
                quote.Foreground = Res("TextFillColorSecondaryBrush");
                return new Border { BorderBrush = Res("TextFillColorTertiaryBrush"), BorderThickness = new Thickness(3, 0, 0, 0), Padding = new Thickness(8, 0, 0, 0), Child = quote };
            case ChatBlock.Rule:
                return new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush"), Margin = new Thickness(0, 2, 0, 2) };
            case ChatBlock.Table t:
                return Table(t, open);
            default:
                return new TextBlock();
        }
    }

    static UIElement Table(ChatBlock.Table t, Action<ChatLink> open)
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
            var cell = Text(t.Header[c].Runs, open, bold: true);
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
                var cell = Text(t.Rows[r].Cells[c].Runs, open);
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

    /// <summary>A block's runs as one text element (selectable; links clickable).</summary>
    internal static RichTextBlock Text(IEnumerable<ChatRun> runs, Action<ChatLink> open, double size = 13.5, bool bold = false)
    {
        var p = new Paragraph();
        foreach (var r in runs)
        {
            var run = new Run { Text = r.Text };
            if (r.Code)
            {
                run.FontFamily = Mono;
                run.FontSize = size - 1;
            }
            if (r.Bold || bold) run.FontWeight = FontWeights.SemiBold;
            if (r.Italic) run.FontStyle = Windows.UI.Text.FontStyle.Italic;
            if (r.Strikethrough) run.TextDecorations = Windows.UI.Text.TextDecorations.Strikethrough;
            if (r.Link is ChatLink link)
            {
                var h = new Hyperlink();
                h.Inlines.Add(run);
                ToolTipService.SetToolTip(h, link.Url);
                h.Click += (_, _) => open(link);
                p.Inlines.Add(h);
            }
            else
            {
                p.Inlines.Add(run);
            }
        }
        var t = new RichTextBlock { FontSize = size, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.Wrap };
        t.Blocks.Add(p);
        return t;
    }
}
