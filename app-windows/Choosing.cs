using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// What every chooser surface draws the same way (the Mac's HeldButtons, CancelControl and ChoiceChecklist): Cancel and
/// Start as the core words them (<c>HeldActions</c>: Shift keeps the .torrent file, its caption under Cancel while Shift
/// is down; Return, Escape and Shift+Escape only where the core lists them), and a chooser's checklist (Select all /
/// none, the selected size, the rows with their folds, tri-state boxes, badges and sizes, the space note). Every tick,
/// fold and key goes to the shared chooser in the core (<see cref="ChoiceStore"/>).
/// </summary>
static class Choosing
{
    static Brush Res(string key) => Theme.Res(key);

    // The checklist that had the keyboard when the core changed it (it is drawn again).
    static string? refocus;

    /// <summary>Whether Shift is down (QA can force it: no key is held in a QA run).</summary>
    internal static bool ShiftDown => QaShift ?? Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(Windows.System.VirtualKey.Shift)
        .HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);

    internal static bool? QaShift { get; set; }

    // The Shift captions shown, and the one timer that looks at Shift for them all: the notifier draws its buttons at every tick,
    // and a timer for each draw was never let go (see Timers).
    static readonly List<TextBlock> shiftCaptions = [];
    static Microsoft.UI.Dispatching.DispatcherQueueTimer? shiftTimer;

    static void ShiftCaptionShown(TextBlock caption)
    {
        caption.Opacity = ShiftDown ? 1 : 0;
        shiftCaptions.Add(caption);
        if (shiftTimer is null)
        {
            shiftTimer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
            shiftTimer.Interval = TimeSpan.FromMilliseconds(100);
            shiftTimer.Tick += (_, _) => { var shown = ShiftDown ? 1 : 0; foreach (var c in shiftCaptions) c.Opacity = shown; };
        }
        shiftTimer.Start();
    }

    static void ShiftCaptionGone(TextBlock caption)
    {
        shiftCaptions.Remove(caption);
        if (shiftCaptions.Count == 0) shiftTimer?.Stop();
    }

    /// <summary>Start and Cancel, right-aligned, in the Windows order (the primary first: the user's decision); Start the accent button.</summary>
    internal static FrameworkElement HeldButtons(HeldActions a, Action<bool> cancel, Action start)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        var cancelButton = new Button { Content = a.Cancel.Title, IsEnabled = a.Cancel.Enabled, MinWidth = 80 };
        ToolTipService.SetToolTip(cancelButton, a.Cancel.Help);
        // Shift is read again at the click: a click before the next look still counts.
        cancelButton.Click += (_, _) => cancel(a.KeepFile is not null && ShiftDown);
        if (a.Cancel.Keys.Contains(HeldKey.Escape))
        {
            cancelButton.KeyboardAccelerators.Add(new KeyboardAccelerator { Key = Windows.System.VirtualKey.Escape });
        }
        var cancelColumn = new StackPanel { Spacing = 2 };
        cancelColumn.Children.Add(cancelButton);
        if (a.KeepFile is HeldKeepFile keep)
        {
            // "Keep .torrent file" under Cancel while Shift is down (looked at every 100 ms while shown).
            var caption = new TextBlock { Text = keep.Caption, FontSize = Theme.Caption, Foreground = Res("TextFillColorSecondaryBrush"), HorizontalAlignment = HorizontalAlignment.Right, Opacity = 0 };
            cancelColumn.Children.Add(caption);
            cancelColumn.Loaded += (_, _) => ShiftCaptionShown(caption);
            cancelColumn.Unloaded += (_, _) => ShiftCaptionGone(caption);
            if (keep.Keys.Contains(HeldKey.ShiftEscape))
            {
                var shiftEscape = new KeyboardAccelerator { Key = Windows.System.VirtualKey.Escape, Modifiers = Windows.System.VirtualKeyModifiers.Shift };
                shiftEscape.Invoked += (_, e) => { e.Handled = true; cancel(true); };
                cancelButton.KeyboardAccelerators.Add(shiftEscape);
            }
        }
        if (a.Start is HeldAction s)
        {
            var startButton = new Button { Content = s.Title, IsEnabled = s.Enabled, MinWidth = 80, VerticalAlignment = VerticalAlignment.Top, Style = (Style)Application.Current.Resources["AccentButtonStyle"] };
            ToolTipService.SetToolTip(startButton, s.Help);
            startButton.Click += (_, _) => start();
            if (s.Keys.Contains(HeldKey.Return))
            {
                startButton.KeyboardAccelerators.Add(new KeyboardAccelerator { Key = Windows.System.VirtualKey.Enter });
            }
            row.Children.Add(startButton);
        }
        row.Children.Add(cancelColumn);
        return row;
    }

    /// <summary>A tri-state box drawn from the core's check; a click asks the core (the box itself never decides).</summary>
    static CheckBox Box(FileCheck check, string help, string value, string name, Action click)
    {
        var box = new CheckBox
        {
            IsThreeState = true,
            IsChecked = check switch { FileCheck.On => true, FileCheck.Off => false, _ => null },
            MinWidth = 0,
            MinHeight = 0,
            Padding = new Thickness(0),
            VerticalAlignment = VerticalAlignment.Center,
        };
        ToolTipService.SetToolTip(box, help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(box, name);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetItemStatus(box, value);
        box.Click += (_, _) => click();
        return box;
    }

    /// <summary>
    /// A chooser's checklist: the notifier's open block (with <paramref name="buttons"/>) and the list under a held row
    /// (without: they sit on the row). It scrolls past <paramref name="maxHeight"/>; Space, Up and Down move the core's
    /// cursor while it has the keyboard.
    /// </summary>
    internal static FrameworkElement Checklist(string id, ChooserPage page, ChoiceStore store, double maxHeight,
        HeldActions? buttons = null, Action<bool>? cancel = null, Action? start = null, FileListButton? showAll = null, Action? toggleShowAll = null)
    {
        var s = new StackPanel { Spacing = 8 };
        var top = new Grid { ColumnSpacing = 10 };
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        top.ColumnDefinitions.Add(new ColumnDefinition());
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var all = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        all.Children.Add(Box(page.All.Check, page.All.CheckHelp, page.All.CheckValue, page.All.Title, () => store.Edit(id, c => c.ToggleAll())));
        var allTitle = new TextBlock { Text = page.All.Title, FontSize = Theme.Secondary, VerticalAlignment = VerticalAlignment.Center };
        allTitle.Tapped += (_, _) => store.Edit(id, c => c.ToggleAll());
        all.Children.Add(allTitle);
        ToolTipService.SetToolTip(all, page.All.Help);
        top.Children.Add(all);
        if (showAll is FileListButton sa && toggleShowAll is not null)
        {
            var link = new HyperlinkButton { Content = sa.Title, FontSize = Theme.Secondary, Padding = new Thickness(4, 2, 4, 2) };
            ToolTipService.SetToolTip(link, sa.Help);
            link.Click += (_, _) => toggleShowAll();
            Grid.SetColumn(link, 2);
            top.Children.Add(link);
        }
        var selected = new TextBlock
        {
            Text = page.SelectedLine,
            FontSize = Theme.Secondary,
            VerticalAlignment = VerticalAlignment.Center,
            Foreground = page.NotEnoughSpace ? Res("SystemFillColorCautionBrush") : Res("TextFillColorSecondaryBrush"),
        };
        Grid.SetColumn(selected, 3);
        top.Children.Add(selected);
        s.Children.Add(top);

        var rows = new StackPanel();
        // The list owns Space, Up and Down while it has the keyboard; the cursor row is lit then.
        var list = new ContentControl { IsTabStop = true, UseSystemFocusVisuals = false, HorizontalContentAlignment = HorizontalAlignment.Stretch };
        bool focused = false;
        void Fill()
        {
            rows.Children.Clear();
            foreach (var r in page.Rows) rows.Children.Add(Row(id, r, store, focused, () => refocus = id));
        }
        Fill();
        list.GotFocus += (_, _) => { focused = true; Fill(); };
        list.LostFocus += (_, _) => { focused = false; Fill(); };
        // A key or a click redraws the list (the core changed it): the new one takes the keyboard back.
        if (refocus == id)
        {
            refocus = null;
            list.Loaded += (_, _) => list.Focus(FocusState.Programmatic);
        }
        list.KeyDown += (_, e) =>
        {
            if (e.Key is Windows.System.VirtualKey.Space or Windows.System.VirtualKey.Down or Windows.System.VirtualKey.Up) refocus = id;
            switch (e.Key)
            {
                case Windows.System.VirtualKey.Space: store.Edit(id, c => c.ToggleCursor()); e.Handled = true; break;
                case Windows.System.VirtualKey.Down: store.Edit(id, c => c.MoveCursor(1)); e.Handled = true; break;
                case Windows.System.VirtualKey.Up: store.Edit(id, c => c.MoveCursor(-1)); e.Handled = true; break;
            }
        };
        list.Content = new ScrollViewer { Content = rows, MaxHeight = maxHeight, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        s.Children.Add(new Border { Child = list, CornerRadius = new CornerRadius(Theme.CodeRadius), Background = Theme.Faint(0.05) });
        if (page.NotEnoughSpace)
        {
            s.Children.Add(new TextBlock { Text = page.SpaceNote, FontSize = Theme.Secondary, Foreground = Res("SystemFillColorCautionBrush"), TextWrapping = TextWrapping.Wrap });
        }
        if (buttons is HeldActions b && cancel is not null && start is not null)
        {
            s.Children.Add(HeldButtons(b, cancel, start));
        }
        return s;
    }

    /// <summary>A row: a group's fold chevron, the box, the title (a click ticks it), its badge and its size.</summary>
    internal static FrameworkElement Row(string id, ChooserRow item, ChoiceStore store, bool listFocused, Action focusList)
    {
        var g = new Grid { Height = 30, ColumnSpacing = 4, Padding = new Thickness(item.Level == 0 ? 4 : 20, 0, 10, 0) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(22) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(26) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        if (item.Level == 0 && item.Foldable)
        {
            var fold = new Button
            {
                Content = new FontIcon { Glyph = item.Open ? "" : "", FontSize = 9 },
                Padding = new Thickness(3),
                MinWidth = 0,
                MinHeight = 0,
                VerticalAlignment = VerticalAlignment.Center,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                BorderThickness = new Thickness(0),
            };
            ToolTipService.SetToolTip(fold, item.FoldHelp);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(fold, item.FoldLabel);
            fold.Click += (_, _) => store.Edit(id, c => c.ToggleFolded(item.Id));
            g.Children.Add(fold);
        }
        var box = Box(item.Check, item.CheckHelp, item.CheckValue, item.Title, () => store.Edit(id, c => c.Toggle(item.Id)));
        Grid.SetColumn(box, 1);
        g.Children.Add(box);
        var title = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center, Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent) };
        title.Children.Add(new TextBlock { Text = item.Title, FontSize = Theme.Secondary, TextTrimming = TextTrimming.CharacterEllipsis, Foreground = Res(item.Dimmed ? "TextFillColorSecondaryBrush" : "TextFillColorPrimaryBrush"), VerticalAlignment = VerticalAlignment.Center });
        if (item.Detail.Length > 0)
        {
            title.Children.Add(new TextBlock { Text = item.Detail, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush"), VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis });
        }
        if (item.Badge is string badge)
        {
            var green = Theme.Color("SystemFillColorSuccessBrush");
            title.Children.Add(new Border { CornerRadius = new CornerRadius(8), Padding = new Thickness(6, 0, 6, 1), Background = Theme.Brush(green, 0.16), VerticalAlignment = VerticalAlignment.Center, Child = new TextBlock { Text = badge, FontSize = Theme.Micro, Foreground = Theme.Brush(green) } });
        }
        // The tap that ticks covers the title only, never the chevron or the box.
        title.Tapped += (_, _) => { focusList(); store.Edit(id, c => c.Toggle(item.Id)); };
        Grid.SetColumn(title, 2);
        g.Children.Add(title);
        var size = new TextBlock { Text = item.Size, FontSize = Theme.Caption, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(size, 3);
        g.Children.Add(size);
        if (listFocused && item.Cursor) g.Background = Theme.Brush(Theme.Accent, 0.18);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetItemStatus(g, item.CheckValue);
        return g;
    }

    /// <summary>A category pill: the category's label as a quiet capsule whose menu re-plans the torrent (the current one checked).</summary>
    internal static FrameworkElement CategoryMenu(string title, string help, IEnumerable<CategoryChoice> choices, Action<Category> choose, bool pill)
    {
        var menu = new MenuFlyout();
        foreach (var c in choices)
        {
            var item = new ToggleMenuFlyoutItem { Text = c.Title, IsChecked = c.Checked };
            var category = c.Category;
            item.Click += (_, _) => choose(category);
            menu.Items.Add(item);
        }
        var b = new Button { Flyout = menu };
        if (pill)
        {
            b.Content = new TextBlock { Text = title, FontSize = Theme.Caption, FontWeight = FontWeights.Medium, Foreground = Res("TextFillColorSecondaryBrush") };
            b.Padding = new Thickness(8, 2, 8, 3);
            b.MinWidth = 0;
            b.MinHeight = 0;
            b.CornerRadius = new CornerRadius(10);
            b.Background = Theme.Faint(0.10);
            b.BorderThickness = new Thickness(0);
            b.VerticalAlignment = VerticalAlignment.Top;
        }
        else
        {
            b.Content = title;
        }
        ToolTipService.SetToolTip(b, help);
        return b;
    }
}
