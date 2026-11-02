using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using System.Runtime.InteropServices.WindowsRuntime;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Visual QA, the Mac's <c>--render-snapshots</c> (app/Sources/SeedwellApp/Support/SnapshotRenderer.swift): the real
/// windows on the simulated library, drawn to PNG without the screen, so an agent can look at every view with the
/// session locked and nobody at the PC. <c>Seedwell.exe --render-snapshots &lt;dir&gt; [--theme light]</c>; one theme
/// per run. Not used in normal operation.
/// <para>
/// The pictures are XAML's own rendering: the acrylic and Mica backdrops are not part of it, so a window is drawn on the
/// theme's solid base colour instead; popups (a row's menu, a dialog) are drawn from the popup layer on their own.
/// </para>
/// </summary>
sealed class SnapshotRenderer(App app, FlyoutWindow flyout, string dir, string theme)
{
    /// <summary>A change laid out and drawn (layout passes and the next frame; no fixed wait).</summary>
    Task Settle()
    {
        flyout.RootElement.UpdateLayout();
        return Quiet.Settle(app.LibraryWindowIfCreated?.Content);
    }

    /// <summary>The theme's solid base colour, standing in for the backdrops and acrylic XAML does not draw.</summary>
    readonly Windows.UI.Color Base = theme == "light" ? Windows.UI.Color.FromArgb(255, 243, 243, 243) : Windows.UI.Color.FromArgb(255, 32, 32, 32);

    public async Task RenderAll()
    {
        Directory.CreateDirectory(dir);
        var root = flyout.RootElement;
        if (root is Panel panel)
        {
            panel.Background = new SolidColorBrush(Base);
        }

        flyout.OpenPinned(withSync: false);
        await Settle();
        await Save(root, "menu");

        flyout.OpenPinned(withSync: true);
        await Settle();
        await Save(root, "menu-sync");

        // Stop syncing's inline question, on the first folder.
        if (app.Core.SyncSnapshot().Folders.FirstOrDefault() is SyncFolder folder)
        {
            app.SyncRemoval.Toggle(folder.Id, await app.Core.SyncOnlyHere(folder.Id));
            flyout.OpenPinned(withSync: true);
            await Settle();
            await Save(root, "menu-sync-confirm");
            app.SyncRemoval.Close();
        }

        flyout.OpenPinned(withSync: false);
        await Settle();
        if (flyout.AnyRow() is TransferRow row)
        {
            // A row's right-click menu.
            var menu = RowCommands.Menu(app.Core, [row], true, root.XamlRoot);
            // In the window, not a window of its own: XAML can only draw what is in its own tree.
            menu.ShouldConstrainToRootBounds = true;
            menu.ShowAt(root, new Windows.Foundation.Point(40, 40));
            await Settle();
            await SavePopups(root.XamlRoot, "row-menu");
            menu.Hide();
            await Settle();

            // The Remove question, both kinds.
            foreach (var (deleteFiles, name) in new[] { (false, "remove"), (true, "remove-delete-files") })
            {
                var d = RowCommands.RemovalDialog(root.XamlRoot, [row], deleteFiles);
                _ = d.ShowAsync();
                await Settle();
                await SavePopups(root.XamlRoot, name);
                d.Hide();
                await Settle();
            }
        }

        await RenderLibrary();
        await RenderChatFiles();
        await RenderFileIcons();

        // The tray icon in its looks, at the tray's sizes (drawn for the taskbar's current theme).
        var looks = new (string Name, IconLook Look)[]
        {
            ("idle", new IconLook(0, 0, null, null, false)),
            ("down", new IconLook(1, 0, 0.4, null, false)),
            ("up", new IconLook(0, 0.5, null, null, false)),
            ("both", new IconLook(0.625, 1, 0.8, null, true)),
            ("moving", new IconLook(0, 0, null, 0.6, false)),
        };
        foreach (var (name, look) in looks)
        {
            foreach (var px in new[] { 16, 24, 32 })
            {
                using var icon = TrayIcon.Draw(look, px);
                using var bmp = icon.ToBitmap();
                bmp.Save(Path.Combine(dir, $"tray-{name}-{px}.png"), System.Drawing.Imaging.ImageFormat.Png);
            }
        }
    }

    /// <summary>
    /// The library window on the mock library (compare with the Mac's shots/mac/downloads-*, sharing-*, tracker-*): each
    /// place the sidebar offers, chosen through the Library as a click would.
    /// </summary>
    async Task RenderLibrary()
    {
        var window = app.LibraryWindow;
        window.ShowAndActivate();
        window.AppWindow.Resize(new Windows.Graphics.SizeInt32(1180, 820));
        var pane = window.Pane;
        if (pane.Root is Panel p)
        {
            p.Background = new SolidColorBrush(Base);
        }
        async Task Place(LibraryPlace place, string name)
        {
            app.Library.SelectPlace(place);
            pane.Refresh();
            await Settle();
            await Save(window.Content, name);
        }
        await Place(new LibraryPlace.Downloads(), "library-downloads");
        await Place(new LibraryPlace.Sharing(), "library-sharing");
        await Place(new LibraryPlace.FolderSync(), "library-foldersync");
        // Settings: Updates, Devices and Folder Sync's Cloudflare rendezvous (off: the guide).
        await Place(new LibraryPlace.Settings(), "library-settings");
        // Every tab of Settings (W5), as its tab button shows it.
        if (pane.SettingsPageShown is SettingsPage settingsPage)
        {
            foreach (var t in new[] { SettingsTab.General, SettingsTab.Transfers, SettingsTab.Folders, SettingsTab.Network, SettingsTab.FolderSync, SettingsTab.Devices, SettingsTab.Agents, SettingsTab.Advanced })
            {
                settingsPage.Show(t);
                await Settle();
                await Save(window.Content, $"settings-{t.ToString().ToLowerInvariant()}");
            }
            settingsPage.Show(SettingsTab.General);
        }
        // Folder Sync's Cloudflare section, on its tab (only there is it in the window).
        if (pane.SettingsPageShown is SettingsPage withRendezvous)
        {
            withRendezvous.Show(SettingsTab.FolderSync);
            await Settle();
            await Save(withRendezvous.Rendezvous.Root, "settings-rendezvous");
            withRendezvous.Show(SettingsTab.General);
        }
        await Place(new LibraryPlace.Downloads(), "library-downloads");
        async Task Menu(MenuFlyout flyout, string name)
        {
            flyout.ShouldConstrainToRootBounds = true;
            flyout.ShowAt(window.Content, new Windows.Foundation.Point(60, 300));
            await PopupOpened(window.Content.XamlRoot);
            await SavePopups(window.Content.XamlRoot, name);
            flyout.Hide();
            await PopupsClosed(window.Content.XamlRoot);
        }
        // The tray's right-click menu (the core's app commands, then Open and Quit), drawn in the window.
        var tray = new MenuFlyout();
        foreach (var item in app.TrayMenuItems())
        {
            tray.Items.Add(item);
        }
        await Menu(tray, "tray-menu");
        // A channel's sidebar menu, as a right-click shows it, and its dialogs as its entries open them.
        if (app.Core.ChatChannels().FirstOrDefault() is SyncChannel channel)
        {
            await Menu(pane.RenderMenu(new LibraryPlace.Chat(channel.Id, null)), "channel-menu");
            foreach (var (dialog, name) in new[]
            {
                (TrsFfiMethods.ChatNewTopicDialog(channel), "dialog-new-topic"),
                (TrsFfiMethods.ChatRenameDialog(channel), "dialog-rename"),
                (TrsFfiMethods.ChatLeaveDialog(channel), "dialog-leave"),
                (TrsFfiMethods.ChatAgentsDialog(channel), "dialog-agents"),
                (TrsFfiMethods.ChatWakeCommandDialog(channel), "dialog-wake-command"),
            })
            {
                app.ChatDialogs.Show(dialog, channel);
                await PopupOpened(window.Content.XamlRoot);
                await SavePopups(window.Content.XamlRoot, name);
                app.ChatDialogs.QaCancel();
                await PopupsClosed(window.Content.XamlRoot);
            }
        }
        // New Channel, both sides; Join with a link that is not one (the core's line under it).
        NewChannelDialog.Show(app, null);
        await PopupOpened(window.Content.XamlRoot);
        if (NewChannelDialog.Open is NewChannelDialog nc)
        {
            await SavePopups(window.Content.XamlRoot, "new-channel");
            nc.QaMode(ChatNewMode.Join);
            nc.QaLink("seedwell://chat/not-a-real-link");
            await Settle();
            await SavePopups(window.Content.XamlRoot, "new-channel-join");
            nc.QaCancel();
            await PopupsClosed(window.Content.XamlRoot);
        }
        // Folder Sync (W3): the page's Stop Syncing question, Add Folder on both sides, a folder's Edit and its Share.
        if (app.Core.SyncSnapshot().Folders.FirstOrDefault() is SyncFolder folder)
        {
            app.Library.SelectPlace(new LibraryPlace.FolderSync());
            pane.Refresh();
            if (pane.SyncPage is FolderSyncPage page)
            {
                page.Choose(folder.Id);
                page.Run(new SyncPageCommand.AskRemove(), folder.Id);
                await Settle();
                await Save(window.Content, "foldersync-remove");
                page.Run(new SyncPageCommand.CancelRemove(), folder.Id);
            }
            AddFolderDialog.Show(app, null);
            await PopupOpened(window.Content.XamlRoot);
            if (AddFolderDialog.Open is AddFolderDialog af)
            {
                await SavePopups(window.Content.XamlRoot, "add-folder");
                af.QaMode(SyncAddMode.Shared);
                af.QaLink("seedwell://sync/not-a-real-link");
                await Settle();
                await SavePopups(window.Content.XamlRoot, "add-folder-shared");
                af.QaCancel();
                await PopupsClosed(window.Content.XamlRoot);
            }
            EditFolderDialog.Show(app, folder.Id);
            await PopupOpened(window.Content.XamlRoot);
            if (EditFolderDialog.Open is EditFolderDialog ef)
            {
                ef.QaOptions(true);
                await Settle();
                await SavePopups(window.Content.XamlRoot, "edit-folder");
                ef.QaCancel();
                await PopupsClosed(window.Content.XamlRoot);
            }
            ShareDialog.Show(app, folder.Id);
            await PopupOpened(window.Content.XamlRoot);
            if (ShareDialog.Open is ShareDialog sd)
            {
                await Settle();
                await SavePopups(window.Content.XamlRoot, "share");
                sd.QaCancel();
                await PopupsClosed(window.Content.XamlRoot);
            }
        }
        // The conversation page (W6): the mock's first channel with its ⋯ menu, and its private conversation (the mock's
        // channels have no members to draw: QA checks the members flyout on a real channel).
        if (app.Core.ChatChannels().FirstOrDefault() is SyncChannel chatChannel)
        {
            await Place(new LibraryPlace.Chat(chatChannel.Id, null), "library-chat");
            if (pane.ChatPageShown is ChatPage chat)
            {
                await Menu(chat.RenderMenu(), "chat-menu");
            }
        }
        if (pane.Model?.Sidebar.Directs?.Items.FirstOrDefault() is SidebarItem chatDirect)
        {
            await Place(chatDirect.Place, "library-direct");
        }
        // The core's questions (W4) as their dialogs, each from its model with a sample input.
        foreach (var (title, text, yes, no, name) in new[]
        {
            Prompt(TrsFfiMethods.AddRequestPrompt(new AddRequestInfo(@"C:\Users\Anna\Downloads\Ubuntu 26.04.torrent", "ubuntu-26.04.1-desktop-amd64.iso", 6_200_000_000, Category.Software, "torrent.ubuntu.com")), "prompt-add-request"),
            TrsFfiMethods.LocationMovePrompt(new MoveProposalInfo([new MoveItemInfo("a", "Film.mkv", 4_000_000_000, @"C:\Users\Anna\Downloads", @"D:\Media", true), new MoveItemInfo("b", "Series", 12_000_000_000, @"C:\Users\Anna\Downloads", @"D:\Media", true)], 16_000_000_000, 2)) is LocationMovePrompt lm
                ? (lm.Title, lm.Text, lm.MoveTitle, lm.DontMoveTitle, "prompt-location-move") : default,
            Offer(TrsFfiMethods.BacklogOffer(5, @"C:\Users\Anna\Downloads", TorrentFileAction.Leave), "prompt-backlog"),
        })
        {
            if (title is null) continue;
            var d = new ContentDialog { XamlRoot = window.Content.XamlRoot, Title = title, Content = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap }, PrimaryButtonText = yes, CloseButtonText = no, DefaultButton = ContentDialogButton.Primary };
            _ = d.ShowAsync();
            await PopupOpened(window.Content.XamlRoot);
            await SavePopups(window.Content.XamlRoot, name);
            d.Hide();
            await PopupsClosed(window.Content.XamlRoot);
        }
        if (pane.Model?.Sidebar.Trackers?.Items.FirstOrDefault() is SidebarItem tracker)
        {
            await Place(tracker.Place, "library-tracker");
        }
        // A category chip: the first row's pill, as its click would.
        await Place(new LibraryPlace.Downloads(), "library-downloads-again");
        if (pane.Model?.List?.Groups.SelectMany(g => g.Rows).FirstOrDefault() is ListRow first)
        {
            app.Library.ToggleCategory(first.Pill.Category);
            await Place(new LibraryPlace.Downloads(), "library-filter");
            app.Library.ClearCategory();
        }
        // Selection (a click, then a Ctrl-click: two rows) and the Remove question under the first, as the clicks would.
        var listed = pane.Model?.List?.Groups.SelectMany(g => g.Rows.Select(r => (g.Key, r.Id))).Take(2).ToArray() ?? [];
        if (listed.Length == 2)
        {
            // Drawn as they are: choosing the place again would clear the selection.
            async Task Now(string name)
            {
                pane.Refresh();
                await Settle();
                await Save(window.Content, name);
            }
            app.Library.ClickRow(listed[0].Key, listed[0].Id, false, false);
            app.Library.ClickRow(listed[1].Key, listed[1].Id, true, false);
            await Now("library-selected");
            app.Library.ClickRow(listed[0].Key, listed[0].Id, false, false);
            app.Library.AskRemoval(app.Library.MenuRows(listed[0].Id), false);
            await Now("library-removal");
            app.Library.CancelRemoval();
        }
        // Get Info on the second row, as its Get Info button would (the card over the page).
        if (listed.Length == 2)
        {
            app.Library.SetInfo(listed[1].Id);
            pane.Refresh();
            await Settle();
            await Save(window.Content, "library-info");
            app.Library.SetInfo(null);
            pane.Refresh();
            await Settle();
        }
        // A folded group: the first that folds, as a click on its title would (unfolded again after).
        if (pane.Model?.List?.Groups.FirstOrDefault(g => g.FoldHelp is not null && !g.Folded) is ListGroup fold)
        {
            app.Library.ToggleFold(fold.Key);
            await Place(new LibraryPlace.Downloads(), "library-folded");
            app.Library.ToggleFold(fold.Key);
        }
    }

    static (string, string, string, string, string) Prompt(AddRequestPrompt p, string name) => (p.Title, p.Text, p.AddTitle, p.DontAddTitle, name);
    static (string, string, string, string, string) Offer(BacklogOffer o, string name) => (o.Title, o.Text, o.AddTitle, o.LaterTitle, name);

    /// <summary>
    /// A file card in every state the core has a sample of (chat_file_samples), the pending sends, and the question
    /// before opening an app, on one sheet.
    /// </summary>
    async Task RenderChatFiles()
    {
        var samples = TrsFfiMethods.ChatFileSamples();
        var attachments = new ChatAttachments(app);
        var sheet = new StackPanel { Padding = new Thickness(16), Spacing = 10, Background = new SolidColorBrush(Base), Width = 900 };
        var grid = new Grid { ColumnSpacing = 16, RowSpacing = 10 };
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        for (int i = 0; i < samples.Cards.Length; i++)
        {
            if (i % 2 == 0) grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var cell = new StackPanel { Spacing = 4 };
            cell.Children.Add(new TextBlock { Text = samples.Cards[i].Label, FontSize = 11, Opacity = 0.6 });
            cell.Children.Add(attachments.File("", samples.Cards[i].Card, () => { }));
            Grid.SetRow(cell, i / 2);
            Grid.SetColumn(cell, i % 2);
            grid.Children.Add(cell);
        }
        sheet.Children.Add(grid);
        sheet.Children.Add(attachments.Pending("", samples.Pending, () => { }));
        var ask = new StackPanel { Spacing = 6, Padding = new Thickness(12), Background = (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"], CornerRadius = new CornerRadius(8) };
        ask.Children.Add(new TextBlock { Text = samples.Confirm.Title, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        ask.Children.Add(new TextBlock { Text = samples.Confirm.Message, TextWrapping = TextWrapping.Wrap });
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        buttons.Children.Add(new Button { Content = samples.Confirm.Open });
        buttons.Children.Add(new Button { Content = samples.Confirm.Cancel, Style = (Style)Application.Current.Resources["AccentButtonStyle"] });
        ask.Children.Add(buttons);
        sheet.Children.Add(ask);
        var window = app.LibraryWindow;
        var before = window.Content;
        window.Content = sheet;
        await Settle();
        await Save(sheet, "chat-files");
        window.Content = before;
        await Settle();
    }

    /// <summary>
    /// The file-type icons from each system image list the rows use by scaling (100 %, 150 %, 200 %), each drawn at
    /// 64 px, so their sharpness can be compared without a high-DPI screen.
    /// </summary>
    async Task RenderFileIcons()
    {
        var sheet = new StackPanel { Padding = new Thickness(16), Spacing = 8, Background = new SolidColorBrush(Base) };
        foreach (var ext in new[] { "", "iso", "mkv", "mp4", "m4b", "mp3", "epub", "pdf", "zip", "exe" })
        {
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 16 };
            row.Children.Add(new TextBlock { Text = ext.Length == 0 ? "(folder)" : ext, Width = 70, VerticalAlignment = VerticalAlignment.Center });
            foreach (var scale in new[] { 1.0, 1.5, 2.0 })
            {
                row.Children.Add(FileIcons.For(ext, scale) is ImageSource source
                    ? new Image { Source = source, Width = 64, Height = 64 }
                    : new TextBlock { Text = "none", Width = 64 });
            }
            sheet.Children.Add(row);
        }
        flyout.RootElement.Visibility = Visibility.Collapsed;
        var window = app.LibraryWindow;
        window.Content = sheet;
        await Settle();
        await Save(sheet, "file-icons");
    }

    /// <summary>Waits until a popup is open and drawn (a dialog opens with an animation), at most 3 s.</summary>
    async Task PopupOpened(XamlRoot root)
    {
        for (int i = 0; i < 60 && !VisualTreeHelper.GetOpenPopupsForXamlRoot(root).Any(p => p.Child is FrameworkElement { ActualWidth: > 0 }); i++)
        {
            await Task.Delay(50);
        }
        await Settle();
    }

    /// <summary>Waits until no popup is open (a hidden dialog closes with an animation), at most 3 s.</summary>
    static async Task PopupsClosed(XamlRoot root)
    {
        for (int i = 0; i < 60 && VisualTreeHelper.GetOpenPopupsForXamlRoot(root).Count > 0; i++)
        {
            await Task.Delay(50);
        }
    }

    /// <summary>Everything in the open popups (a menu, a dialog), one picture each.</summary>
    async Task SavePopups(XamlRoot root, string name)
    {
        int i = 0;
        foreach (var p in VisualTreeHelper.GetOpenPopupsForXamlRoot(root))
        {
            // A popup still closing (a dialog just hidden) has no size left: nothing to draw.
            if (p.Child is FrameworkElement { ActualWidth: > 0, ActualHeight: > 0 } child)
            {
                await Save(child, i == 0 ? name : $"{name}-{i}");
                i++;
            }
        }
        if (i == 0)
        {
            throw new InvalidOperationException($"{name}: no popup opened");
        }
    }

    Task Save(UIElement element, string name) => SaveImage(element, Path.Combine(dir, $"{name}-{theme}.png"), theme == "light");

    /// <summary>An element drawn to a PNG, laid on the theme's base colour (the QA commands' shot uses it too).</summary>
    internal static async Task SaveImage(UIElement element, string path, bool light)
    {
        var back = light ? Windows.UI.Color.FromArgb(255, 243, 243, 243) : Windows.UI.Color.FromArgb(255, 32, 32, 32);
        var rtb = new RenderTargetBitmap();
        await rtb.RenderAsync(element);
        var pixels = (await rtb.GetPixelsAsync()).ToArray();
        // Nothing drawn (a popup with nothing in it yet, a light-dismiss layer): no picture, rather than an empty file.
        if (rtb.PixelWidth == 0 || rtb.PixelHeight == 0 || pixels.Length == 0)
        {
            return;
        }
        // Acrylic (a menu's background) and the dialog's smoke come out transparent: lay the picture on the base colour.
        for (int i = 0; i < pixels.Length; i += 4)
        {
            int keep = 255 - pixels[i + 3];
            pixels[i] = (byte)(pixels[i] + back.B * keep / 255);
            pixels[i + 1] = (byte)(pixels[i + 1] + back.G * keep / 255);
            pixels[i + 2] = (byte)(pixels[i + 2] + back.R * keep / 255);
            pixels[i + 3] = 255;
        }
        using var file = File.Create(path);
        var encoder = await Windows.Graphics.Imaging.BitmapEncoder.CreateAsync(Windows.Graphics.Imaging.BitmapEncoder.PngEncoderId, file.AsRandomAccessStream());
        var dpi = 96 * element.XamlRoot.RasterizationScale;
        encoder.SetPixelData(Windows.Graphics.Imaging.BitmapPixelFormat.Bgra8, Windows.Graphics.Imaging.BitmapAlphaMode.Premultiplied,
            (uint)rtb.PixelWidth, (uint)rtb.PixelHeight, dpi, dpi, pixels);
        await encoder.FlushAsync();
    }
}
