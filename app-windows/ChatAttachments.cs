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

    // ---------------------------------------------------------------------------------------------- images

    /// <summary>The highest an image is shown (the Mac's ChatImageLayout: the content width and 420 high).</summary>
    const double MaxHeight = 420;
    /// <summary>A gallery's filmstrip: 46 px squares, the picture inset 5 px, 4 px apart.</summary>
    const double Thumb = 46, ThumbInset = 5;

    /// <summary>Where a picture comes from: the core (<c>chat_image_view</c>), or a fixture's (the snapshots' samples).</summary>
    internal delegate Task<ChatImageView?> ViewSource(ChatImage image, uint side);

    /// <summary>
    /// An image's size: at most <paramref name="maxWidth"/> wide and 420 high, its aspect kept, and never larger than its
    /// own pixels at the screen's <paramref name="scale"/> (no upscaling).
    /// </summary>
    internal static (double W, double H) Fit(uint width, uint height, double maxWidth, double scale)
    {
        double w = Math.Max(width, 1u) / scale, h = Math.Max(height, 1u) / scale;
        var k = Math.Min(Math.Min(maxWidth / w, MaxHeight / h), 1);
        return (Math.Max(12, Math.Round(w * k)), Math.Max(12, Math.Round(h * k)));
    }

    /// <summary>The pixels to ask for a picture shown at <paramref name="w"/> × <paramref name="h"/>: in steps, so a redraw at a close size reuses it.</summary>
    static uint SideFor(ChatImage image, double w, double h, double scale)
    {
        var want = (uint)Math.Ceiling(Math.Max(w, h) * scale / 64) * 64;
        return Math.Max(64, Math.Min(want, Math.Max(image.Width, image.Height)));
    }

    /// <summary>
    /// A message's images, at its top: one alone (fitted, on <paramref name="side"/>), or a gallery for several
    /// (<paramref name="gallery"/>, the core's). <paramref name="active"/> is the image a gallery shows (view state, the
    /// page's); the returned <c>Show</c> switches it (an image reference's chip), and <paramref name="selected"/> hears it.
    /// </summary>
    internal (FrameworkElement View, Action<int> Show) Images(string place, ChatImage[] images, bool gallery, double maxWidth, double scale,
        HorizontalAlignment side, int active, Action<int> selected, Action changed, ViewSource? source = null)
    {
        source ??= (image, px) => app.Core.ChatImageView(place, image.Hash, px);
        if (!gallery || images.Length < 2)
        {
            var one = new StackPanel { Spacing = 8, HorizontalAlignment = side };
            foreach (var i in images)
            {
                var (w, h) = Fit(i.Width, i.Height, maxWidth, scale);
                one.Children.Add(Image(place, i, w, h, scale, source, changed));
            }
            return (one, _ => { });
        }
        return Gallery(place, images, maxWidth, scale, side, active, selected, changed, source);
    }

    /// <summary>
    /// A gallery (Finder's Gallery view): a stage as tall as the tallest image fitted (switching never changes the
    /// height) with the active image centred, the arrows over its edges while the pointer is on it, the core's "2 / 5"
    /// at its top-right, and a filmstrip under it; Left and Right switch while it has focus.
    /// </summary>
    (FrameworkElement, Action<int>) Gallery(string place, ChatImage[] images, double maxWidth, double scale, HorizontalAlignment side,
        int active, Action<int> selected, Action changed, ViewSource source)
    {
        var current = Math.Clamp(active, 0, images.Length - 1);
        var stageHeight = images.Max(i => Fit(i.Width, i.Height, maxWidth, scale).H);
        var stage = new Grid { Height = stageHeight, Width = maxWidth, Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent) };
        var strip = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, HorizontalAlignment = HorizontalAlignment.Center };
        var thumbs = new List<Grid>();
        var counter = new TextBlock { FontSize = Theme.Micro, FontWeight = FontWeights.SemiBold, Foreground = new SolidColorBrush(Microsoft.UI.Colors.White) };
        var previous = Arrow("", HorizontalAlignment.Left);
        var next = Arrow("", HorizontalAlignment.Right);
        var picture = new Border { HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
        stage.Children.Add(picture);
        stage.Children.Add(new Border
        {
            Child = counter,
            Padding = new Thickness(7, 1, 7, 2),
            CornerRadius = new CornerRadius(8),
            Background = new SolidColorBrush(Microsoft.UI.Colors.Black) { Opacity = 0.45 },
            HorizontalAlignment = HorizontalAlignment.Right,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(0, 8, 8, 0),
        });
        stage.Children.Add(previous);
        stage.Children.Add(next);
        void Show(int n)
        {
            current = (n % images.Length + images.Length) % images.Length;
            var i = images[current];
            var (w, h) = Fit(i.Width, i.Height, maxWidth, scale);
            picture.Child = Image(place, i, w, h, scale, source, changed);
            counter.Text = TrsFfiMethods.ChatGalleryCounter((uint)current + 1, (uint)images.Length);
            for (int t = 0; t < thumbs.Count; t++)
            {
                var on = t == current;
                thumbs[t].Background = on ? Theme.Brush(Theme.Accent, 0.22) : null;
                thumbs[t].BorderThickness = new Thickness(on ? 2 : 0);
                thumbs[t].Opacity = on ? 1 : 0.6;
            }
            selected(current);
        }
        previous.Click += (_, _) => Show(current - 1);
        next.Click += (_, _) => Show(current + 1);
        for (int n = 0; n < images.Length; n++)
        {
            var i = images[n];
            var index = n;
            var cell = new Grid { Width = Thumb, Height = Thumb, CornerRadius = new CornerRadius(7), BorderBrush = Theme.Brush(Theme.Accent) };
            var small = new Image { Stretch = Stretch.Uniform, Margin = new Thickness(ThumbInset) };
            if (i.State == ChatImageState.Available)
            {
                var px = SideFor(i, Thumb, Thumb, scale);
                Load(small, $"{place}/{i.Hash}/view/{px}", () => source(i, px), 1);
            }
            else if (i.HasPreview)
            {
                Load(small, $"{place}/{i.Hash}/preview", () => app.Core.ChatImagePreview(place, i.Hash), 0.7);
            }
            cell.Children.Add(small);
            ToolTipService.SetToolTip(cell, i.Caption);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(cell, i.Caption);
            cell.Tapped += (_, _) => Show(index);
            cell.PointerEntered += (_, _) => cell.Opacity = 1;
            cell.PointerExited += (_, _) => { if (index != current) cell.Opacity = 0.6; };
            thumbs.Add(cell);
            strip.Children.Add(cell);
        }
        var all = new StackPanel { Spacing = 6 };
        all.Children.Add(stage);
        all.Children.Add(new ScrollViewer
        {
            Content = strip,
            MaxWidth = maxWidth,
            HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
            VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
            HorizontalScrollMode = ScrollMode.Auto,
        });
        // A control of its own, so Left and Right reach it.
        var view = new ContentControl { Content = all, IsTabStop = true, UseSystemFocusVisuals = true, HorizontalAlignment = side, HorizontalContentAlignment = HorizontalAlignment.Stretch };
        view.KeyDown += (_, e) =>
        {
            if (e.Key == Windows.System.VirtualKey.Left) { Show(current - 1); e.Handled = true; }
            else if (e.Key == Windows.System.VirtualKey.Right) { Show(current + 1); e.Handled = true; }
        };
        stage.PointerEntered += (_, _) => previous.Opacity = next.Opacity = 1;
        stage.PointerExited += (_, _) => previous.Opacity = next.Opacity = 0;
        strip.Tapped += (_, _) => view.Focus(FocusState.Pointer);
        Show(current);
        return (view, Show);
    }

    /// <summary>A gallery's previous or next: a round button over the stage's edge, shown while the pointer is on the stage.</summary>
    static Button Arrow(string glyph, HorizontalAlignment edge) => new()
    {
        Content = new FontIcon { Glyph = glyph, FontSize = 12 },
        Width = 32,
        Height = 32,
        Padding = new Thickness(0),
        CornerRadius = new CornerRadius(16),
        Opacity = 0,
        HorizontalAlignment = edge,
        VerticalAlignment = VerticalAlignment.Center,
        Margin = new Thickness(8, 0, 8, 0),
        Background = Theme.Res("AcrylicInAppFillColorDefaultBrush"),
    };

    /// <summary>One image at <paramref name="w"/> × <paramref name="h"/>, rounded, no caption (name and size in its tooltip); a click opens it.</summary>
    FrameworkElement Image(string place, ChatImage image, double w, double h, double scale, ViewSource source, Action changed)
    {
        var frame = new Grid { Width = w, Height = h, CornerRadius = new CornerRadius(Theme.ImageRadius), Background = Res("SubtleFillColorSecondaryBrush") };
        var picture = new Image { Stretch = Stretch.Uniform };
        frame.Children.Add(picture);
        if (image.State == ChatImageState.Available)
        {
            var px = SideFor(image, w, h, scale);
            Load(picture, $"{place}/{image.Hash}/view/{px}", () => source(image, px), 1);
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
            over.Children.Add(new TextBlock { Text = image.Status, FontSize = Theme.Caption, TextWrapping = TextWrapping.Wrap, TextAlignment = TextAlignment.Center, MaxWidth = Math.Max(40, w - 36) });
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
        return frame;
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
