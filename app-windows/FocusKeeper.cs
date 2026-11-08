using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Seedwell;

/// <summary>
/// A dialog that draws itself anew keeps the keyboard's place: its controls are made again, which would drop the focus, so
/// the same control (its kind and label, else its place among those of its kind) gets it back, and a text box its caret. A
/// refresh while the user is typing never takes their focus. Focus outside what is drawn anew is left alone.
/// </summary>
static class FocusKeeper
{
    static string Label(Control c) => c switch
    {
        ToggleSwitch t => t.Header as string ?? "",
        ContentControl { Content: string s } => s,
        _ => AutomationProperties.GetName(c),
    };

    static Control[] Controls(UIElement scope) => [.. RowCommands.Tree(scope).OfType<Control>()];

    static Control? Focused(UIElement scope, XamlRoot? root)
    {
        if (root is null || FocusManager.GetFocusedElement(root) is not Control c) return null;
        // Up from it, a few steps: down from the scope would look at every element, on each redraw, and a wrapper with memory
        // pressure for each is what brought the app its full collections.
        for (DependencyObject? d = c; d is not null; d = VisualTreeHelper.GetParent(d))
        {
            if (ReferenceEquals(d, scope)) return c;
        }
        return null;
    }

    /// <summary>Runs <paramref name="draw"/>, the focus and caret in <paramref name="scope"/> as they were.</summary>
    internal static void Redraw(UIElement scope, XamlRoot? root, Action draw)
    {
        if (Focused(scope, root) is not Control was)
        {
            draw();
            return;
        }
        var before = Controls(scope);
        var (kind, label, state) = (was.GetType(), Label(was), was.FocusState == FocusState.Unfocused ? FocusState.Programmatic : was.FocusState);
        var ofLabel = Array.IndexOf(before.Where(c => c.GetType() == kind && Label(c) == label).ToArray(), was);
        var ofKind = Array.IndexOf(before.Where(c => c.GetType() == kind).ToArray(), was);
        var caret = was is TextBox box ? (box.SelectionStart, box.SelectionLength) : (0, 0);
        draw();
        // Laid out first: a control not in the tree yet cannot take the focus.
        scope.UpdateLayout();
        var after = Controls(scope).Where(c => c.GetType() == kind).ToArray();
        var same = after.Where(c => Label(c) == label).ElementAtOrDefault(ofLabel) ?? after.ElementAtOrDefault(ofKind);
        if (same is null) return;
        same.Focus(state);
        if (same is TextBox again) again.Select(Math.Min(caret.Item1, again.Text.Length), caret.Item2);
    }

    /// <summary>QA: what in <paramref name="scope"/> has the focus, and a text box's caret.</summary>
    internal static string Describe(UIElement scope, XamlRoot? root) => Focused(scope, root) switch
    {
        null => "none",
        TextBox t => $"{nameof(TextBox)} caret {t.SelectionStart}",
        Control c => $"{c.GetType().Name} {Label(c)}",
    };

    /// <summary>
    /// QA, the guard against a new surface losing the keyboard: each text box, password box and link of
    /// <paramref name="scope"/>, and the first of every other kind of control, takes the focus; <paramref name="redraw"/>
    /// forces a redraw; the same one must have it after. Answers how many were tried and which lost it.
    /// </summary>
    internal static async Task<string> QaWalk(UIElement scope, XamlRoot? root, Action redraw)
    {
        static bool Always(Control c) => c is TextBox or PasswordBox or HyperlinkButton;
        static Control[] Walkable(UIElement s) => [.. Controls(s).Where(c => c.IsEnabled && c.IsTabStop && c.Visibility == Visibility.Visible)];
        var seen = new HashSet<Type>();
        var chosen = new List<(Type Kind, int Index)>();
        foreach (var c in Walkable(scope))
        {
            if (Always(c) || seen.Add(c.GetType())) chosen.Add((c.GetType(), Walkable(scope).Where(o => o.GetType() == c.GetType()).ToList().IndexOf(c)));
        }
        var lost = new List<string>();
        var tried = 0;
        foreach (var (kind, index) in chosen)
        {
            if (Walkable(scope).Where(c => c.GetType() == kind).ElementAtOrDefault(index) is not Control c) continue;
            if (!c.Focus(FocusState.Keyboard)) continue;
            tried++;
            var label = Label(c);
            await Quiet.Settle(scope);
            // What has it once settled: a group may hand it to its selected item, and the surface's own refresh may have made
            // the same control anew. By kind and place, then, not the object. None: its own refresh took it.
            if (Focused(scope, root) is not Control held)
            {
                lost.Add($"{kind.Name}#{index} {label} -> none, on its own");
                continue;
            }
            var (heldKind, heldIndex) = (held.GetType(), Walkable(scope).Where(o => o.GetType() == held.GetType()).ToList().IndexOf(held));
            redraw();
            await Quiet.Settle(scope);
            var now = Focused(scope, root);
            if (now is null || now.GetType() != heldKind || Walkable(scope).Where(o => o.GetType() == heldKind).ToList().IndexOf(now) != heldIndex)
            {
                lost.Add($"{heldKind.Name}#{heldIndex} {label} -> {(now is null ? "none" : $"{now.GetType().Name} {Label(now)}")}");
            }
        }
        return $"walk {tried} controls, lost {lost.Count} [{string.Join(" | ", lost)}]";
    }

    /// <summary>QA: the first button of <paramref name="scope"/> takes the focus.</summary>
    internal static bool QaFocusFirstButton(UIElement scope) => Controls(scope).OfType<Microsoft.UI.Xaml.Controls.Primitives.ButtonBase>().FirstOrDefault()?.Focus(FocusState.Keyboard) == true;

    /// <summary>QA: the first control in <paramref name="scope"/> whose label starts so takes the focus.</summary>
    internal static bool QaFocus(UIElement scope, string label) => Controls(scope).FirstOrDefault(c => Label(c).StartsWith(label, StringComparison.Ordinal))?.Focus(FocusState.Keyboard) == true;
}
