using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The library window's Folder Sync page, a first version until the Mac's page model lands (its steps 2-6): the folders
/// on the left (the Folder Sync menu's rows: badge, name, status), and the chosen folder on the right (name, path, the
/// core's status line, this PC's role, its members with the core's member lines and roles, and its issues). Every text
/// is the core's. NEED from Rust: the page model itself (selection, section titles "Members" / "Conflicts" / "Issues",
/// the "(this PC)" marker, the member's display name, Share… / Edit… / Stop Syncing… and conflict actions).
/// </summary>
sealed class FolderSyncPage
{
    readonly App app;
    readonly Grid root = new() { ColumnSpacing = 0 };
    readonly StackPanel list = new() { Padding = new Thickness(12, 4, 12, 12), Spacing = 2 };
    readonly StackPanel detail = new() { Padding = new Thickness(24, 8, 24, 24), Spacing = 6 };
    string? chosen;
    string drawn = "";

    public UIElement Root => root;

    public FolderSyncPage(App app)
    {
        this.app = app;
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(320) });
        root.ColumnDefinitions.Add(new ColumnDefinition());
        root.Children.Add(new ScrollViewer { Content = list });
        var right = new ScrollViewer { Content = detail };
        Grid.SetColumn(right, 1);
        root.Children.Add(right);
        Update();
    }

    /// <summary>Redraws from the core's sync snapshot when anything in it changed (asked on every model refresh).</summary>
    public void Update()
    {
        SyncSnapshot snapshot;
        try { snapshot = app.Core.SyncSnapshot(); } catch { return; }
        // In the core's order (the Folder Sync menu's rows), not the snapshot's.
        var order = TrsFfiMethods.SyncMenuModel(snapshot, app.SyncRemoval, App.HostWindows).Rows.Select((r, i) => (r.Id, i)).ToDictionary(x => x.Id, x => x.i);
        var folders = snapshot.Folders.OrderBy(f => order.TryGetValue(f.Id, out var i) ? i : int.MaxValue).ToArray();
        if (chosen is null || folders.All(f => f.Id != chosen))
        {
            chosen = folders.FirstOrDefault()?.Id;
        }
        FollowLine? follow = null;
        if (chosen is string id)
        {
            try { follow = app.Core.SyncFollowLine(id); } catch (Exception e) { AppLog.Write($"follow line failed: {e.Message}"); }
        }
        var key = string.Join("|", folders.Select(f => $"{f.Id}:{TrsFfiMethods.SyncFolderSubtitle(f)}:{f.Connected}:{f.Members}")) + $"#{chosen}#{follow}";
        if (key == drawn)
        {
            return;
        }
        drawn = key;
        list.Children.Clear();
        foreach (var f in folders)
        {
            list.Children.Add(FolderRow(f, f.Id == chosen));
        }
        detail.Children.Clear();
        if (folders.FirstOrDefault(f => f.Id == chosen) is SyncFolder folder)
        {
            Detail(folder, snapshot, follow);
        }
    }

    /// <summary>
    /// "On all my devices" (SPEC §7G.1 "Your devices", the Mac's FollowToggle): the core's title, the line under it, its
    /// help as the tooltip, and whether it may be switched.
    /// </summary>
    UIElement Follow(string id, FollowLine line)
    {
        var s = new StackPanel { Spacing = 2, Margin = new Thickness(0, 8, 0, 0) };
        var toggle = new ToggleSwitch { Header = line.Title, IsOn = line.On, IsEnabled = line.Enabled };
        if (line.Help.Length > 0)
        {
            ToolTipService.SetToolTip(toggle, line.Help);
        }
        toggle.Toggled += (_, _) => SetFollow(id, toggle.IsOn);
        s.Children.Add(toggle);
        if (line.Line.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = line.Line, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        }
        return s;
    }

    /// <summary>Puts the folder on all the user's devices, or stops that (nothing is deleted anywhere).</summary>
    internal void SetFollow(string id, bool on)
    {
        try
        {
            app.Core.SyncSetFollow(id, on);
        }
        catch (CoreException e)
        {
            // The core's sentence as the alert's message, as the Mac's.
            app.Alert(new SyncAlert(TrsFfiMethods.CoreErrorMessage(e), ""));
        }
        drawn = "";
        Update();
    }

    /// <summary>The folder shown on the right (QA).</summary>
    internal string? Chosen => chosen;

    /// <summary>Shows folder <paramref name="id"/> on the right (a seedwell://folder/ link), as a click on its row.</summary>
    internal void Choose(string id)
    {
        chosen = id;
        drawn = "";
        Update();
    }

    UIElement FolderRow(SyncFolder f, bool on)
    {
        var g = new Grid { ColumnSpacing = 10, Padding = new Thickness(10, 8, 10, 8), CornerRadius = new CornerRadius(6) };
        g.Background = on ? (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"] : new SolidColorBrush(Colors.Transparent);
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(20) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        var badge = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        FlyoutWindow.SetBadge(badge, TrsFfiMethods.SyncFolderBadge(f));
        g.Children.Add(badge);
        var text = new StackPanel();
        text.Children.Add(new TextBlock { Text = f.Name, FontWeight = on ? FontWeights.SemiBold : FontWeights.Normal, TextTrimming = TextTrimming.CharacterEllipsis });
        text.Children.Add(new TextBlock { Text = TrsFfiMethods.SyncFolderSubtitle(f), FontSize = 12, Opacity = 0.65, TextTrimming = TextTrimming.CharacterEllipsis });
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        var id = f.Id;
        g.Tapped += (_, _) => { chosen = id; drawn = ""; Update(); };
        return g;
    }

    void Detail(SyncFolder f, SyncSnapshot snapshot, FollowLine? follow)
    {
        detail.Children.Add(new TextBlock { Text = f.Name, FontSize = 22, FontWeight = FontWeights.SemiBold });
        // The path in the secondary colour, a magnifier after it that shows the folder in File Explorer (the Mac's).
        var pathLine = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        pathLine.Children.Add(new TextBlock { Text = f.Path, FontSize = 12, Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"], VerticalAlignment = VerticalAlignment.Center, IsTextSelectionEnabled = true });
        var reveal = new Button { Content = new FontIcon { Glyph = "", FontSize = 12 }, Padding = new Thickness(4), Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
        // NEED: the magnifier's help and name from Rust ("Show in File Explorer").
        reveal.Click += (_, _) => System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"\"{f.Path}\"") { UseShellExecute = true });
        pathLine.Children.Add(reveal);
        detail.Children.Add(pathLine);
        detail.Children.Add(new TextBlock { Text = $"{TrsFfiMethods.SyncFolderSubtitle(f)} · {TrsFfiMethods.SyncRoleLabel(f.Role)}", Opacity = 0.75, TextWrapping = TextWrapping.Wrap });
        if (f.Error is string error)
        {
            detail.Children.Add(new TextBlock { Text = error, Foreground = (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"], TextWrapping = TextWrapping.Wrap });
        }
        if (follow is { Shown: true } fl)
        {
            detail.Children.Add(Follow(f.Id, fl));
        }

        SyncMember[] members;
        try { members = TrsFfiMethods.SyncMembersOrdered(app.Core.SyncMembers(f.Id)); } catch { members = []; }
        if (members.Length > 0)
        {
            detail.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Margin = new Thickness(0, 14, 0, 6), Fill = (Brush)Application.Current.Resources["DividerStrokeColorDefaultBrush"] });
        }
        foreach (var m in members)
        {
            var device = snapshot.Devices.FirstOrDefault(d => d.Id == m.DeviceId);
            var row = new Grid { ColumnSpacing = 12, Padding = new Thickness(0, 6, 0, 6) };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.Children.Add(new FontIcon { Glyph = m.IsMe ? "" : m.Online ? "" : "", FontSize = 16, Opacity = m.Online || m.IsMe ? 1 : 0.5, VerticalAlignment = VerticalAlignment.Center });
            var who = new StackPanel();
            who.Children.Add(new TextBlock { Text = m.Name, FontWeight = m.IsMe ? FontWeights.SemiBold : FontWeights.Normal });
            var line = TrsFfiMethods.SyncMemberLine(m, device, null);
            if (line.Length > 0)
            {
                who.Children.Add(new TextBlock { Text = line, FontSize = 12, Opacity = 0.65 });
            }
            Grid.SetColumn(who, 1);
            row.Children.Add(who);
            var role = new TextBlock { Text = TrsFfiMethods.SyncRoleLabel(m.Role), Opacity = 0.7, VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(role, 2);
            row.Children.Add(role);
            detail.Children.Add(row);
        }

        if (f.Issues.Length > 0)
        {
            detail.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Margin = new Thickness(0, 14, 0, 6), Fill = (Brush)Application.Current.Resources["DividerStrokeColorDefaultBrush"] });
        }
        foreach (var issue in f.Issues)
        {
            var s = new StackPanel { Padding = new Thickness(0, 4, 0, 4) };
            // An empty path: the issue is the whole folder's (its reason alone).
            if (issue.Path.Length > 0)
            {
                s.Children.Add(new TextBlock { Text = issue.Path });
            }
            s.Children.Add(new TextBlock { Text = issue.Reason, FontSize = 12, Opacity = 0.65, TextWrapping = TextWrapping.Wrap });
            detail.Children.Add(s);
        }
    }
}
