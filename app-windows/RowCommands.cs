using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// A row's commands: the list is the core's (<c>row_commands</c>, the same as the Mac's); this draws it as a menu and
/// runs what is picked. Commands that need a window the Windows app does not have yet (Get Info, Move To, the file
/// chooser, rules) open a placeholder; Play has no players to offer yet.
/// </summary>
static class RowCommands
{
    /// <summary>
    /// The menu for these rows (the tray flyout's right click: <paramref name="inMenuBar"/>). <paramref name="remove"/>:
    /// where Remove… and Remove and Delete Files… go instead of the dialog (the library window's question under the row,
    /// the Mac's strip).
    /// </summary>
    public static MenuFlyout Menu(Core core, TransferRow[] rows, bool inMenuBar, XamlRoot root, Action<TransferRow[], bool>? remove = null)
    {
        TorrentInfo? info = null;
        if (rows.Length == 1)
        {
            try { info = core.Info(rows[0].Id); } catch { }
        }
        RuleRecord[] rules = [];
        try { rules = core.RulesLoad().Rules; } catch { }
        // Play and Play With from the core's offer (its players, the row's videos); the chooser is here (Choose Files…).
        PlayOffer? play = null;
        if (rows.Length == 1) { try { play = core.PlayOffer(rows[0]); } catch (Exception e) { AppLog.Write($"play offer failed: {e.Message}"); } }
        var list = SeedwellFfiMethods.RowCommands(rows, info, rules, play, inMenuBar, true, App.HostWindows);
        var menu = new MenuFlyout();
        foreach (var item in Draw(core, list, rows, root, remove))
        {
            menu.Items.Add(item);
        }
        return menu;
    }

    /// <summary>
    /// A held row's buttons (the row's countdown or choice actions), in the core's words: Start first, then Cancel (the
    /// Windows order, primary first: the user's decision)
    /// from the row's commands (StartNow while it counts down, ConfirmChoice while it waits for a choice; CancelHeld).
    /// </summary>
    public static CommandEntry.Item[] Held(TransferRow row) =>
        SeedwellFfiMethods.RowCommands([row], null, [], null, false, true, App.HostWindows)
            .OfType<CommandEntry.Item>()
            .Where(i => i.Command is RowCommand.CancelHeld or RowCommand.StartNow or RowCommand.ConfirmChoice)
            .OrderBy(i => i.Command is RowCommand.CancelHeld ? 1 : 0)
            .ToArray();

    /// <summary>A menu of these entries (Get Info's menus and Play With), built when it opens on its anchor.</summary>
    internal static MenuFlyout MenuFrom(Core core, CommandEntry[] entries, TransferRow[] rows, FrameworkElement anchor)
    {
        var menu = new MenuFlyout();
        menu.Opening += (_, _) =>
        {
            menu.Items.Clear();
            foreach (var item in Draw(core, entries, rows, anchor.XamlRoot, null))
            {
                menu.Items.Add(item);
            }
        };
        return menu;
    }

    static IEnumerable<MenuFlyoutItemBase> Draw(Core core, CommandEntry[] list, TransferRow[] rows, XamlRoot root, Action<TransferRow[], bool>? remove)
    {
        foreach (var entry in list)
        {
            switch (entry)
            {
                case CommandEntry.Separator:
                    yield return new MenuFlyoutSeparator();
                    break;
                case CommandEntry.Note n:
                    yield return new MenuFlyoutItem { Text = n.Title, IsEnabled = false };
                    break;
                case CommandEntry.Submenu sub:
                    var m = new MenuFlyoutSubItem { Text = sub.Title };
                    foreach (var i in Draw(core, sub.Entries, rows, root, remove))
                    {
                        m.Items.Add(i);
                    }
                    yield return m;
                    break;
                case CommandEntry.Item it:
                    MenuFlyoutItem item = it.Checked ? new ToggleMenuFlyoutItem { IsChecked = true } : new MenuFlyoutItem();
                    item.Text = it.Title;
                    item.IsEnabled = it.Enabled;
                    if (it.Destructive)
                    {
                        // As the Mac's: the destructive command in red.
                        item.Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["SystemFillColorCriticalBrush"];
                    }
                    var command = it.Command;
                    var title = it.Title;
                    item.Click += async (_, _) =>
                    {
                        // Remove and Remove and Delete Files ask first, in the core's words. An async void handler must not
                        // let an exception out (it would end the app): one that stops the question also stops the removal.
                        if (remove is not null && command is RowCommand.Remove or RowCommand.RemoveAndDeleteFiles)
                        {
                            remove(rows, command is RowCommand.RemoveAndDeleteFiles);
                            return;
                        }
                        try
                        {
                            if (command is RowCommand.Remove or RowCommand.RemoveAndDeleteFiles
                                && !await ConfirmRemove(root, rows, command is RowCommand.RemoveAndDeleteFiles))
                            {
                                return;
                            }
                        }
                        catch (Exception e)
                        {
                            AppLog.Write($"removal question failed: {e.Message}");
                            return;
                        }
                        Run(core, command, rows, root);
                    };
                    yield return item;
                    break;
            }
        }
    }

    /// <summary>
    /// The Remove question (the core's <c>removal_question</c>): its message, Cancel as the default and the confirm
    /// button drawn as destructive, with a bin icon when the files go too.
    /// </summary>
    static async Task<bool> ConfirmRemove(XamlRoot root, TransferRow[] rows, bool deleteFiles) =>
        await RemovalDialog(root, rows, deleteFiles).ShowAsync() == ContentDialogResult.Primary;

    /// <summary>The Remove question's dialog, not yet shown.</summary>
    internal static ContentDialog RemovalDialog(XamlRoot root, TransferRow[] rows, bool deleteFiles)
    {
        var q = SeedwellFfiMethods.RemovalQuestion(rows, deleteFiles);
        var d = new ContentDialog
        {
            XamlRoot = root,
            PrimaryButtonText = q.ConfirmTitle,
            CloseButtonText = q.CancelTitle,
            // As the Mac: Remove or Delete is red and Enter; Cancel is plain and Esc.
            DefaultButton = ContentDialogButton.Primary,
            // The question is the text, beside its icon, wrapping as long as it needs: a title is cut at two lines.
            Content = Question(q.Message, q.DeleteFiles ? "" : ""),
        };
        // The destructive look (the Mac's red button) on the confirm button, and each button's help as its tooltip.
        Destructive(d);
        d.Opened += (_, _) =>
        {
            foreach (var b in DialogButtons(d))
            {
                if (Equals(b.Content, q.ConfirmTitle)) ToolTipService.SetToolTip(b, q.ConfirmHelp);
                else if (Equals(b.Content, q.CancelTitle)) ToolTipService.SetToolTip(b, q.CancelHelp);
            }
        };
        return d;
    }

    static UIElement Question(string message, string glyph)
    {
        var g = new Grid { ColumnSpacing = 16 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new FontIcon { Glyph = glyph, FontSize = 32, VerticalAlignment = VerticalAlignment.Top });
        var text = new TextBlock { Text = message, TextWrapping = TextWrapping.Wrap, Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"] };
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        return g;
    }

    /// <summary>
    /// The dialog's default (confirm) button in red with white text, in every state. A ContentDialog gives its default
    /// button the accent style whatever PrimaryButtonStyle says, so the accent button's own colours are set red here, in
    /// the dialog's resources, which that style reads; only the default button uses them.
    /// </summary>
    internal static void Destructive(ContentDialog d)
    {
        Microsoft.UI.Xaml.Media.SolidColorBrush Brush(byte r, byte g, byte b) => new(Windows.UI.Color.FromArgb(255, r, g, b));
        var white = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.White);
        d.Resources["AccentButtonBackground"] = Brush(196, 43, 28);
        d.Resources["AccentButtonBackgroundPointerOver"] = Brush(214, 60, 44);
        d.Resources["AccentButtonBackgroundPressed"] = Brush(168, 35, 22);
        d.Resources["AccentButtonBorderBrush"] = Brush(196, 43, 28);
        d.Resources["AccentButtonBorderBrushPointerOver"] = Brush(214, 60, 44);
        d.Resources["AccentButtonBorderBrushPressed"] = Brush(168, 35, 22);
        d.Resources["AccentButtonForeground"] = white;
        d.Resources["AccentButtonForegroundPointerOver"] = white;
        d.Resources["AccentButtonForegroundPressed"] = white;
    }

    /// <summary>Everything in the open popups (a dialog lives in the popup layer, not under the window's content).</summary>
    internal static IEnumerable<DependencyObject> Descendants(XamlRoot root) =>
        Microsoft.UI.Xaml.Media.VisualTreeHelper.GetOpenPopupsForXamlRoot(root).SelectMany(p => Tree(p.Child));

    /// <summary>
    /// A dialog's own buttons, without a look at what it shows: they are outside its scrolled content, and each element a walk
    /// looks at gets a wrapper with memory pressure of its own (full collections, see SettingsPage.Render).
    /// </summary>
    internal static IEnumerable<Button> DialogButtons(ContentDialog dialog)
    {
        var left = new Stack<DependencyObject>();
        left.Push(dialog);
        while (left.TryPop(out var at))
        {
            if (at is Button b) yield return b;
            if (at is ScrollViewer) continue;
            for (int i = 0; i < Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(at); i++) left.Push(Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(at, i));
        }
    }

    internal static IEnumerable<DependencyObject> Tree(DependencyObject? o)
    {
        if (o is null)
        {
            yield break;
        }
        yield return o;
        for (int i = 0; i < Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(o); i++)
        {
            foreach (var d in Tree(Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(o, i)))
            {
                yield return d;
            }
        }
    }

    internal static void Run(Core core, RowCommand command, TransferRow[] rows, XamlRoot root)
    {
        var ids = rows.Select(r => r.Id).ToArray();
        var first = rows[0];
        // The engine's commands: in order, off the UI thread, the window drawn again once each returned.
        void InOrder(Func<Task> work) => CoreCalls.InOrder($"row command {command.GetType().Name}", work, () => App.Current?.Refreshed());
        try
        {
            switch (command)
            {
                case RowCommand.Pause: InOrder(() => core.Pause(ids)); break;
                case RowCommand.Resume: InOrder(() => core.Resume(ids)); break;
                case RowCommand.StartNow: InOrder(() => core.PendingStartNow(first.Id)); break;
                // Start of a torrent that waits for a choice: the shared chooser's ticks (the core loads it first when
                // nothing showed it, with its default ticks: everything but the Extras).
                case RowCommand.ConfirmChoice:
                    if (App.Current?.Choices is ChoiceStore choices) choices.Confirm(first.Id); else InOrder(() => core.ConfirmChoiceDefault(first.Id));
                    break;
                case RowCommand.OpenChooser:
                    if (App.Current is App app) ChooserDialog.Show(app, first.Id);
                    break;
                // Choose Files… of a torrent counting down: its countdown ends and the chooser opens (a magnet link's once
                // its files are known).
                case RowCommand.ChooseFiles:
                    CoreCalls.InOrder("choose files", () => core.PendingChoose(first.Id), open =>
                    {
                        if (open && App.Current is App a) ChooserDialog.Show(a, first.Id);
                        App.Current?.Refreshed();
                    });
                    break;
                // Removed, and its .torrent file to the Recycle Bin unless Shift is down (SPEC 7A.6); a file that could not
                // go is a quiet notification.
                case RowCommand.CancelHeld:
                    var keep = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(Windows.System.VirtualKey.Shift)
                        .HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
                    CoreCalls.InOrder("cancel held", () => core.CancelHeld(first.Id, keep), note =>
                    {
                        if (note is CancelNote n) App.Current?.Notify(n.Title, n.Body);
                        App.Current?.Refreshed();
                    });
                    break;
                case RowCommand.PlayRow: if (App.Current is App pa) Player.Play(pa, first); break;
                case RowCommand.Play p: if (App.Current is App pb) Player.Play(pb, first, p.FileIndex, p.PlayerId); break;
                case RowCommand.StopSharing: InOrder(() => core.StopSharing(ids)); break;
                case RowCommand.MoveToTop: InOrder(() => core.MoveToTop(first.Id)); break;
                case RowCommand.Verify: InOrder(() => core.Verify(ids)); break;
                case RowCommand.AskTrackerAgain a: InOrder(() => core.AskTrackerAgain(a.Ids)); break;
                case RowCommand.SetSeedingGoal g: InOrder(() => core.SetSeedingGoal(ids, g.Goal)); break;
                case RowCommand.SetPriority p: InOrder(() => core.SetPriority(ids, p.Priority)); break;
                case RowCommand.SetCategory c: InOrder(() => core.SetCategory(ids, c.Category)); break;
                case RowCommand.SetRule r: InOrder(() => core.SetRule(ids, r.RuleId)); break;
                case RowCommand.Remove: InOrder(() => core.Remove(ids, false)); break;
                case RowCommand.RemoveAndDeleteFiles: InOrder(() => core.Remove(ids, true)); break;
                case RowCommand.Reveal: Reveal(rows); break;
                case RowCommand.GetInfo: App.Current?.OpenInfo(first.Id); break;
                case RowCommand.CopyMagnet:
                    var data = new Windows.ApplicationModel.DataTransfer.DataPackage();
                    data.SetText(core.MagnetLink(first.Id));
                    Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(data);
                    break;
                case RowCommand.MoveTo: _ = MoveTo(core, ids); break;
                default:
                    // The core offers only commands whose window Windows has (App.HostWindows).
                    AppLog.Write($"row command {command.GetType().Name}: not offered on Windows");
                    break;
            }
        }
        catch (Exception e)
        {
            AppLog.Write($"row command {command.GetType().Name} failed: {e.Message}");
        }
    }

    /// <summary>
    /// "Move to…": the torrents go below a folder the user picks and stay pinned there (SPEC §7A.5, the Mac's moveTo).
    /// </summary>
    internal static async Task MoveTo(Core core, string[] ids)
    {
        if (App.Current is not App app || await app.PickFolder() is not string folder)
        {
            return;
        }
        try
        {
            var r = await core.MoveTo(ids, folder);
            AppLog.Write($"move to {folder}: {r.Queued} queued, {r.Refused.Length} refused");
            foreach (var reason in r.Refused)
            {
                AppLog.Write($"move refused: {reason}");
            }
            if (SeedwellFfiMethods.MoveFailed(r) is SyncAlert alert)
            {
                app.Alert(alert);
            }
        }
        catch (CoreException e)
        {
            AppLog.Write($"move failed: {SeedwellFfiMethods.CoreErrorMessage(e)}");
            app.Alert(SeedwellFfiMethods.MoveError(e));
        }
        app.Refreshed();
    }

    /// <summary>
    /// Shows the rows in File Explorer with every one selected (their save paths, as the Mac's Finder reveal of all
    /// the rows): one Explorer window per parent folder.
    /// </summary>
    public static void Reveal(IEnumerable<TransferRow> rows)
    {
        foreach (var folder in rows.Select(r => r.SavePath).Where(p => p.Length > 0).GroupBy(p => Path.GetDirectoryName(p) ?? p))
        {
            var parent = ILCreateFromPathW(folder.Key);
            if (parent == IntPtr.Zero)
            {
                continue;
            }
            var items = folder.Select(ILCreateFromPathW).Where(p => p != IntPtr.Zero).ToArray();
            try
            {
                SHOpenFolderAndSelectItems(parent, (uint)items.Length, items.Select(ILFindLastID).ToArray(), 0);
            }
            finally
            {
                foreach (var p in items)
                {
                    ILFree(p);
                }
                ILFree(parent);
            }
        }
    }

    [System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    static extern IntPtr ILCreateFromPathW(string path);

    [System.Runtime.InteropServices.DllImport("shell32.dll")]
    static extern IntPtr ILFindLastID(IntPtr pidl);

    [System.Runtime.InteropServices.DllImport("shell32.dll")]
    static extern void ILFree(IntPtr pidl);

    [System.Runtime.InteropServices.DllImport("shell32.dll")]
    static extern int SHOpenFolderAndSelectItems(IntPtr folder, uint count, IntPtr[] items, uint flags);
}
