using System.Runtime.InteropServices.WindowsRuntime;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Images and files in chat on Windows (the Mac's ChatImages and ChatFiles): drawing only. Each image's state and words
/// and each file card's state, line and buttons are the core's (trs-ffi chat_images.rs, chat_files.rs). Every picture
/// drawn is the PNG or JPEG the core made (<c>chat_image_view</c>, <c>chat_image_preview</c>), never another device's
/// bytes as they came. Nothing downloads or opens by itself; opening an app or a script asks the core's question first.
/// </summary>
sealed class ChatAttachments
{
    readonly App app;
    // The pictures the core made, kept while the app runs (a redraw makes none again).
    readonly Dictionary<string, BitmapImage> pictures = [];
    readonly List<string> order = [];
    ChatImageTexts? texts;

    public ChatAttachments(App app) => this.app = app;

    internal ChatImageTexts Texts => texts ??= app.Core.ChatImageTexts();

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    /// <summary>The largest an image is shown, its aspect kept (the Mac's ChatImageLayout), and the pixels asked for it.</summary>
    const double MaxWidth = 420, MaxHeight = 320;
    const uint Side = 840;

    internal static (double W, double H) Fit(uint width, uint height)
    {
        double w = Math.Max(width, 1u), h = Math.Max(height, 1u);
        var scale = Math.Min(Math.Min(MaxWidth / w, MaxHeight / h), 1);
        return (Math.Max(40, Math.Round(w * scale)), Math.Max(40, Math.Round(h * scale)));
    }

    // ---------------------------------------------------------------------------------------------- images

    /// <summary>A message's images, one under the other.</summary>
    internal StackPanel Images(string place, IEnumerable<ChatImage> images, Action changed)
    {
        var s = new StackPanel { Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
        foreach (var i in images)
        {
            s.Children.Add(Image(place, i, changed));
        }
        return s;
    }

    UIElement Image(string place, ChatImage image, Action changed)
    {
        var (w, h) = Fit(image.Width, image.Height);
        var frame = new Grid { Width = w, Height = h, HorizontalAlignment = HorizontalAlignment.Left, CornerRadius = new CornerRadius(6), BorderThickness = new Thickness(1), BorderBrush = Res("ControlStrokeColorDefaultBrush"), Background = Res("SubtleFillColorSecondaryBrush") };
        var picture = new Image { Stretch = Stretch.Uniform };
        frame.Children.Add(picture);
        if (image.State == ChatImageState.Available)
        {
            Load(picture, $"{place}/{image.Hash}/view", () => app.Core.ChatImageView(place, image.Hash, Side), 1);
        }
        else
        {
            if (image.HasPreview)
            {
                Load(picture, $"{place}/{image.Hash}/preview", () => app.Core.ChatImagePreview(place, image.Hash), 0.7);
            }
            var over = new StackPanel { Spacing = 6, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
            if (image.State == ChatImageState.Fetching)
            {
                over.Children.Add(new ProgressRing { IsActive = true, Width = 16, Height = 16 });
            }
            over.Children.Add(new TextBlock { Text = image.Status, FontSize = 11.5, TextWrapping = TextWrapping.Wrap, TextAlignment = TextAlignment.Center, MaxWidth = Math.Max(40, w - 36) });
            if (image.State is ChatImageState.NotDownloaded or ChatImageState.Failed)
            {
                var download = new Button { Content = Texts.Download, HorizontalAlignment = HorizontalAlignment.Center };
                download.Click += (_, _) => Fetch(place, image.Hash, changed);
                over.Children.Add(download);
            }
            frame.Children.Add(new Border { Child = over, Padding = new Thickness(10), Margin = new Thickness(8), CornerRadius = new CornerRadius(8), Background = Res("AcrylicInAppFillColorDefaultBrush"), HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center });
        }
        ToolTipService.SetToolTip(frame, image.Caption);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(frame, image.Caption);
        frame.Tapped += (_, _) => { if (image.State == ChatImageState.Available) OpenImage(place, image.Hash); };
        frame.ContextFlyout = ImageMenu(place, image, changed);
        var s = new StackPanel { Spacing = 3, HorizontalAlignment = HorizontalAlignment.Left };
        s.Children.Add(frame);
        s.Children.Add(new TextBlock { Text = image.Caption, FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = w });
        return s;
    }

    MenuFlyout ImageMenu(string place, ChatImage image, Action changed)
    {
        var m = new MenuFlyout();
        void Add(string title, Action run)
        {
            var item = new MenuFlyoutItem { Text = title };
            item.Click += (_, _) => run();
            m.Items.Add(item);
        }
        if (image.State == ChatImageState.Available)
        {
            Add(Texts.Open, () => OpenImage(place, image.Hash));
            Add(Texts.CopyImage, () => CopyImage(place, image.Hash));
            m.Items.Add(new MenuFlyoutSeparator());
            Add(Texts.SaveAs, () => SaveImage(place, image.Hash, false));
            if (image.Kind == ChatImageKind.Svg) Add(Texts.SaveAsPng, () => SaveImage(place, image.Hash, true));
        }
        else if (image.State is ChatImageState.NotDownloaded or ChatImageState.Failed)
        {
            Add(Texts.Download, () => Fetch(place, image.Hash, changed));
        }
        return m;
    }

    /// <summary>Draws the core's picture into <paramref name="target"/>, made off the UI thread once and kept.</summary>
    void Load(Image target, string key, Func<Task<ChatImageView?>> make, double opacity)
    {
        target.Opacity = opacity;
        if (pictures.TryGetValue(key, out var kept))
        {
            target.Source = kept;
            return;
        }
        _ = Task.Run(make).ContinueWith(t => target.DispatcherQueue.TryEnqueue(async () =>
        {
            if (!t.IsCompletedSuccessfully || t.Result is not ChatImageView v) return;
            var bitmap = await Bitmap(v.Bytes);
            if (bitmap is null) return;
            pictures[key] = bitmap;
            order.Add(key);
            while (order.Count > 150) { pictures.Remove(order[0]); order.RemoveAt(0); }
            target.Source = bitmap;
        }));
    }

    internal static async Task<BitmapImage?> Bitmap(byte[] bytes)
    {
        try
        {
            using var stream = new Windows.Storage.Streams.InMemoryRandomAccessStream();
            using (var writer = new Windows.Storage.Streams.DataWriter(stream.GetOutputStreamAt(0)))
            {
                writer.WriteBytes(bytes);
                await writer.StoreAsync();
                await writer.FlushAsync();
                writer.DetachStream();
            }
            stream.Seek(0);
            var b = new BitmapImage();
            await b.SetSourceAsync(stream);
            return b;
        }
        catch (Exception e)
        {
            AppLog.Write($"chat image not drawn: {e.Message}");
            return null;
        }
    }

    internal void Fetch(string place, string hash, Action changed) =>
        Background(() => app.Core.ChatImageFetch(place, hash), $"image fetch {hash[..Math.Min(8, hash.Length)]}", changed);

    /// <summary>Open: the file the core writes for the default viewer.</summary>
    internal void OpenImage(string place, string hash)
    {
        _ = Task.Run(() => app.Core.ChatImageOpenPath(place, hash)).ContinueWith(t => app.Dispatch(() =>
        {
            if (!t.IsCompletedSuccessfully) { Failed(t.Exception); return; }
            AppLog.Write("chat image: open");
            if (!Quiet.On) System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(t.Result) { UseShellExecute = true });
        }));
    }

    async void CopyImage(string place, string hash)
    {
        var v = await app.Core.ChatImageView(place, hash, 0);
        if (v is null || Quiet.On) return;
        using var stream = new Windows.Storage.Streams.InMemoryRandomAccessStream();
        await stream.WriteAsync(v.Bytes.AsBuffer());
        var package = new Windows.ApplicationModel.DataTransfer.DataPackage();
        package.SetBitmap(Windows.Storage.Streams.RandomAccessStreamReference.CreateFromStream(stream.CloneStream()));
        Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
    }

    async void SaveImage(string place, string hash, bool asPng)
    {
        var name = app.Core.ChatImageSaveName(place, hash, asPng);
        if (await app.PickSaveFile(name) is not string path) return;
        Background(() => app.Core.ChatImageSave(place, hash, path, asPng), "image saved", null);
    }

    // ----------------------------------------------------------------------------------------------- files

    internal static string FileGlyph(ChatFileIcon icon) => icon switch
    {
        ChatFileIcon.Document => "",
        ChatFileIcon.Text => "",
        ChatFileIcon.Pdf => "",
        ChatFileIcon.Image => "",
        ChatFileIcon.Audio => "",
        ChatFileIcon.Video => "",
        ChatFileIcon.Archive => "",
        ChatFileIcon.Code => "",
        ChatFileIcon.App => "",
        ChatFileIcon.DiskImage => "",
        ChatFileIcon.Spreadsheet => "",
        ChatFileIcon.Presentation => "",
        _ => "",
    };

    /// <summary>A message's files, one card under the other.</summary>
    internal StackPanel Files(string place, IEnumerable<ChatFileCard> files, Action changed)
    {
        var s = new StackPanel { Spacing = 6, Margin = new Thickness(0, 4, 0, 0) };
        foreach (var f in files)
        {
            s.Children.Add(File(place, f, changed));
        }
        return s;
    }

    /// <summary>One file: its icon, name, line, progress while it downloads, and its buttons (the first in the accent style).</summary>
    internal UIElement File(string place, ChatFileCard card, Action changed)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(30) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new FontIcon { Glyph = FileGlyph(card.Icon), FontSize = 22, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Top });
        var s = new StackPanel { Spacing = 3 };
        s.Children.Add(new TextBlock { Text = card.Name, FontSize = 12.5, FontWeight = FontWeights.Medium, TextTrimming = TextTrimming.CharacterEllipsis });
        s.Children.Add(new TextBlock
        {
            Text = card.Status,
            FontSize = 11,
            TextWrapping = TextWrapping.Wrap,
            MaxLines = 2,
            Foreground = card.State is ChatFileState.Failed or ChatFileState.Gone ? Res("SystemFillColorCriticalBrush") : Res("TextFillColorSecondaryBrush"),
        });
        if (card.Progress is double p)
        {
            s.Children.Add(new ProgressBar { Value = p * 100, Maximum = 100 });
        }
        if (card.Actions.Length > 0)
        {
            var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(0, 2, 0, 0) };
            for (int n = 0; n < card.Actions.Length; n++)
            {
                var a = card.Actions[n];
                var b = new Button { Content = a.Label, FontSize = 12, Padding = new Thickness(8, 3, 8, 4) };
                if (n == 0) b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
                b.Click += (_, _) => RunFile(place, card, a.Kind, changed);
                buttons.Children.Add(b);
            }
            s.Children.Add(buttons);
        }
        Grid.SetColumn(s, 1);
        g.Children.Add(s);
        var border = new Border
        {
            Child = g,
            Padding = new Thickness(10),
            MaxWidth = 380,
            HorizontalAlignment = HorizontalAlignment.Left,
            CornerRadius = new CornerRadius(8),
            BorderThickness = new Thickness(1),
            BorderBrush = Res("ControlStrokeColorDefaultBrush"),
            Background = Res("SubtleFillColorSecondaryBrush"),
        };
        ToolTipService.SetToolTip(border, card.Status);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(border, $"{card.Name}, {card.Status}");
        return border;
    }

    /// <summary>What a file card's button does (also the QA command chat-file).</summary>
    internal async void RunFile(string place, ChatFileCard card, ChatFileActionKind kind, Action changed)
    {
        var hash = card.Hash;
        AppLog.Write($"chat file {hash[..Math.Min(8, hash.Length)]}: {kind}");
        switch (kind)
        {
            case ChatFileActionKind.SaveAs:
                if (await app.PickSaveFile(app.Core.ChatFileSaveName(place, hash)) is not string path) return;
                Background(() => app.Core.ChatFileDownload(place, hash, path), "file save as", changed);
                break;
            case ChatFileActionKind.ShowInFolder:
                if (card.Path is string shown)
                {
                    if (Quiet.On) AppLog.Write("explorer: the downloaded file");
                    else System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"/select,\"{shown}\"") { UseShellExecute = true });
                }
                break;
            case ChatFileActionKind.Open:
                ChatFileOpen open;
                try { open = await Task.Run(() => app.Core.ChatFileOpen(place, hash)); }
                catch (Exception e) { Failed(new AggregateException(e)); return; }
                if (open.Confirm is ChatFileConfirm ask && !await Confirm(ask)) return;
                AppLog.Write("chat file: open");
                if (!Quiet.On) System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(open.Path) { UseShellExecute = true });
                break;
            case ChatFileActionKind.Download or ChatFileActionKind.Retry:
                Background(() => app.Core.ChatFileDownload(place, hash, null), "file download", changed);
                break;
            case ChatFileActionKind.Cancel:
                Background(() => app.Core.ChatFileCancel(place, hash), "file cancel", changed);
                break;
            case ChatFileActionKind.StopSharing:
                Background(() => app.Core.ChatFileStopSharing(place, hash), "file stop sharing", changed);
                break;
        }
    }

    /// <summary>QA: the last question asked before opening (its title), and the answer a QA run gives.</summary>
    internal static string? Asked { get; private set; }
    internal static bool QaOpenAnswer { get; set; }

    /// <summary>The core's question before opening an app or a script: Open first, Cancel the default.</summary>
    async Task<bool> Confirm(ChatFileConfirm ask)
    {
        Asked = ask.Title;
        AppLog.Write($"chat file asks: {ask.Title}");
        if (Quiet.On || app.LibraryWindow.Content.XamlRoot is not XamlRoot root)
        {
            return QaOpenAnswer;
        }
        var d = new ContentDialog
        {
            XamlRoot = root,
            Title = ask.Title,
            Content = new TextBlock { Text = ask.Message, TextWrapping = TextWrapping.Wrap },
            PrimaryButtonText = ask.Open,
            CloseButtonText = ask.Cancel,
            DefaultButton = ContentDialogButton.Close,
        };
        try { return await d.ShowAsync() == ContentDialogResult.Primary; }
        catch (Exception e) { AppLog.Write($"chat file question failed: {e.Message}"); return false; }
    }

    /// <summary>A core call off the UI thread; a failure is an alert with the core's sentence; then <paramref name="changed"/>.</summary>
    void Background(Func<Task> work, string what, Action? changed)
    {
        _ = Task.Run(work).ContinueWith(t => app.Dispatch(() =>
        {
            AppLog.Write($"chat: {what} {(t.IsCompletedSuccessfully ? "done" : "failed")}");
            if (!t.IsCompletedSuccessfully) Failed(t.Exception);
            changed?.Invoke();
        }));
    }

    void Failed(AggregateException? e)
    {
        var message = e?.InnerException is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : e?.InnerException?.Message ?? "";
        app.Alert(new SyncAlert(message, ""));
    }

    // -------------------------------------------------------------------------------- the composer's parts

    /// <summary>Sends whose files are being read, or that failed: a line each above the field.</summary>
    internal StackPanel Pending(string place, IEnumerable<ChatPendingSend> pending, Action changed)
    {
        var s = new StackPanel { Spacing = 4 };
        foreach (var p in pending)
        {
            var g = new Grid { ColumnSpacing = 8 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            if (p.Failed)
            {
                g.Children.Add(new FontIcon { Glyph = "", FontSize = 12, Foreground = Res("SystemFillColorCautionBrush"), VerticalAlignment = VerticalAlignment.Top });
                var text = new StackPanel { Spacing = 1 };
                text.Children.Add(new TextBlock { Text = p.Title, FontSize = 11.5, FontWeight = FontWeights.Medium });
                text.Children.Add(new TextBlock { Text = p.Problem, FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, MaxLines = 4 });
                Grid.SetColumn(text, 1);
                g.Children.Add(text);
                var dismiss = new Button { Content = p.Dismiss, FontSize = 12, Padding = new Thickness(8, 3, 8, 4) };
                var id = p.Id;
                dismiss.Click += (_, _) => { app.Core.ChatDismissPending(place, id); changed(); };
                Grid.SetColumn(dismiss, 2);
                g.Children.Add(dismiss);
            }
            else
            {
                g.Children.Add(new ProgressBar { Value = p.Progress * 100, Maximum = 100, Width = 80, VerticalAlignment = VerticalAlignment.Center });
                var title = new TextBlock { Text = p.Title, FontSize = 11.5, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis };
                Grid.SetColumn(title, 1);
                g.Children.Add(title);
            }
            s.Children.Add(g);
        }
        return s;
    }

    /// <summary>An attachment waiting in the composer: its picture (or file icon), its caption, and Remove.</summary>
    internal UIElement Draft(ChatImageDraft d, Action remove)
    {
        var g = new Grid { ColumnSpacing = 6, Padding = new Thickness(6), CornerRadius = new CornerRadius(6), Background = Res("SubtleFillColorSecondaryBrush") };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        if (!d.File && d.PreviewPng.Length > 0)
        {
            var image = new Image { Width = 40, Height = 40, Stretch = Stretch.UniformToFill };
            _ = SetPreview(image, d.PreviewPng);
            g.Children.Add(image);
        }
        else
        {
            g.Children.Add(new FontIcon { Glyph = FileGlyph(d.Icon), FontSize = 20, Foreground = Res("TextFillColorSecondaryBrush") });
        }
        var caption = new TextBlock { Text = d.Caption, FontSize = 11, MaxWidth = 220, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(caption, 1);
        g.Children.Add(caption);
        var x = new Button { Content = new FontIcon { Glyph = "", FontSize = 10 }, Padding = new Thickness(4), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
        ToolTipService.SetToolTip(x, Texts.Remove);
        x.Click += (_, _) => remove();
        Grid.SetColumn(x, 2);
        g.Children.Add(x);
        return g;
    }

    static async Task SetPreview(Image image, byte[] png)
    {
        if (await Bitmap(png) is BitmapImage b) image.Source = b;
    }
}
