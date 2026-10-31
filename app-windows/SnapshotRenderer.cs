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
            app.SyncRemoval.Toggle(folder.Id, app.Core.SyncOnlyHere(folder.Id));
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

    /// <summary>Everything in the open popups (a menu, a dialog), one picture each.</summary>
    async Task SavePopups(XamlRoot root, string name)
    {
        int i = 0;
        foreach (var p in VisualTreeHelper.GetOpenPopupsForXamlRoot(root))
        {
            if (p.Child is UIElement child)
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
