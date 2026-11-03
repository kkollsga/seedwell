using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Favorite trackers (SPEC 7E.1, 8.3; the Mac's FavoriteEditing): the core's <c>FavoriteEditor</c> decides the one
/// inline edit in progress (a sidebar name or an address), its validation and red hint, the quiet note after a save and
/// the Remove from Favorites question; this mirrors its view, keeps the one text field (so typing keeps the caret
/// across redraws) and does what each change asks of the window (<c>FavoriteEffect</c>).
/// </summary>
sealed class FavoriteEditing
{
    readonly App app;
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer noteTimer;
    TextBox? field;
    FavoriteTarget? fieldTarget;

    internal FavoriteEditor Editor { get; }

    internal FavoriteEditorView View { get; private set; }

    /// <summary>What is drawn may have changed (the edit, its hint, the note, the question, a save).</summary>
    internal event Action? Changed;

    public FavoriteEditing(App app)
    {
        this.app = app;
        Editor = new FavoriteEditor(app.Core);
        View = Editor.View();
        noteTimer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        noteTimer.IsRepeating = false;
        noteTimer.Tick += (_, _) => Refresh();
    }

    internal void BeginRename(string id) { Editor.BeginRename(id); Refresh(); }
    internal void Cancel() { Editor.Cancel(); Refresh(); }
    internal void AskRemove(string id) => Apply(Editor.AskRemove(id));
    internal void CancelRemoval() { Editor.CancelRemoval(); Refresh(); }
    internal void SetFolderDraft(string id, ProfileRecord record) { Editor.SetFolderDraft(id, record); Refresh(); }

    // The saves: off the UI thread, in order; the window follows when each is done.
    internal void Commit() => Save("favorite commit", () => Editor.Commit());
    internal void ConfirmRemoval() => Save("favorite remove", () => Editor.ConfirmRemoval());
    /// <summary>"Save as Favorite" on an automatic tracker.</summary>
    internal void SaveAsFavorite(string groupId) => Save("favorite save", () => Editor.SaveAsFavorite(groupId));
    internal void SetLimits(string id, ulong down, ulong up) => Save("favorite limits", () => Editor.SetLimits(id, down, up));
    internal void Run(string id, TrackerInfoCommand command) => Save($"tracker info {command.GetType().Name}", () => Editor.TrackerInfoRun(id, command));

    void Save(string what, Func<Task<FavoriteEffect>> work) => CoreCalls.InOrder(what, work, Apply);

    /// <summary>Does what a change asks (the favorite's tab, the library read again), then reads the view again.</summary>
    void Apply(FavoriteEffect effect)
    {
        Refresh();
        if (effect.OpenTab is string id)
        {
            app.Library.SelectPlace(new LibraryPlace.Tracker(id));
            app.Library.SetTrackerInfo(false);
        }
        if (effect.Changed || effect.OpenTab is not null) app.Refreshed();
    }

    /// <summary>Reads the view again (and again when its note has gone); tells what draws it.</summary>
    internal void Refresh()
    {
        View = Editor.View();
        noteTimer.Stop();
        if (View.RefreshInMs is uint ms)
        {
            noteTimer.Interval = TimeSpan.FromMilliseconds(ms);
            noteTimer.Start();
        }
        Changed?.Invoke();
    }

    /// <summary>
    /// The field of <paramref name="target"/> (the one edit in progress): it shows the core's text, sends each change,
    /// commits on Return and when it loses the keyboard, cancels on Escape, and takes the keyboard when it appears.
    /// The same field across redraws while the target stays.
    /// </summary>
    internal TextBox Field(FavoriteTarget target, string prompt = "", bool mono = false)
    {
        if (field is null || !Equals(fieldTarget, target))
        {
            var box = new TextBox { Text = View.Text, PlaceholderText = prompt, MinHeight = 0, Padding = new Thickness(6, 2, 6, 3), BorderBrush = Theme.Brush(Theme.Accent, 0.7) };
            if (mono) box.FontFamily = Theme.MonoFont;
            box.Tag = View.Text;
            box.TextChanged += (_, _) =>
            {
                if (box.Tag as string == box.Text) return;
                box.Tag = box.Text;
                var before = View;
                Editor.SetText(box.Text);
                View = Editor.View();
                // A new hint (or one gone) is drawn; a keystroke alone redraws nothing.
                if (before.Hint != View.Hint) Changed?.Invoke();
            };
            box.KeyDown += (_, e) =>
            {
                if (e.Key == Windows.System.VirtualKey.Enter) { e.Handled = true; Commit(); }
                else if (e.Key == Windows.System.VirtualKey.Escape) { e.Handled = true; Cancel(); }
            };
            box.LostFocus += (_, _) => { if (Equals(View.Target, target) && Equals(fieldTarget, target)) Commit(); };
            box.Loaded += (_, _) => box.Focus(FocusState.Programmatic);
            field = box;
            fieldTarget = target;
        }
        // Out of where it was drawn before (its visual parent: the logical one can be gone already).
        (field.Parent as Panel ?? VisualTreeHelper.GetParent(field) as Panel)?.Children.Remove(field);
        if (field.Parent is Border b) b.Child = null;
        return field;
    }

    /// <summary>QA: types into the field where it is drawn (its TextChanged sends it, as a user's typing does).</summary>
    internal bool QaType(string text)
    {
        if (field is null) return false;
        field.Text = text;
        return true;
    }

    /// <summary>The field's red hint under it, when the core has one.</summary>
    internal FrameworkElement? Hint() => View.Hint is string h
        ? new TextBlock { Text = h, FontSize = Theme.Caption, Foreground = Theme.Res("SystemFillColorCriticalBrush"), Margin = new Thickness(4, 2, 0, 0), TextWrapping = TextWrapping.Wrap }
        : null;

    /// <summary>
    /// The Remove from Favorites question as a strip (over the tracker's list, on its Info page): the star-slash, the
    /// sentence, the red Remove first and Cancel (the Windows order).
    /// </summary>
    internal UIElement RemovalStrip(FavoriteRemoval q, Action confirm, Action cancel)
    {
        var g = new Grid { ColumnSpacing = 12, Padding = new Thickness(14, 10, 12, 10), CornerRadius = new CornerRadius(6), Background = Theme.Res("SubtleFillColorSecondaryBrush") };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = "", FontSize = 15, VerticalAlignment = VerticalAlignment.Center });
        var text = new TextBlock { Text = q.Message, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var remove = new Button { Content = q.ConfirmTitle, MinWidth = 72, Foreground = Theme.Res("SystemFillColorCriticalBrush") };
        ToolTipService.SetToolTip(remove, q.ConfirmHelp);
        remove.Click += (_, _) => confirm();
        var no = new Button { Content = q.CancelTitle, MinWidth = 72 };
        ToolTipService.SetToolTip(no, q.CancelHelp);
        no.Click += (_, _) => cancel();
        buttons.Children.Add(remove);
        buttons.Children.Add(no);
        Grid.SetColumn(buttons, 2);
        g.Children.Add(buttons);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, q.Message);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetLiveSetting(g, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Assertive);
        return g;
    }
}
