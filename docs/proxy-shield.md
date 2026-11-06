# The proxy icon: a shield with the proxy logo

One icon shows the proxy's state: a shield with a logo inside, a path that enters from the left, passes through a
relay (the ring) and leaves forward (the arrow). The Mac draws it in code (`app/Sources/SeedwellApp/Support/ProxyShield.swift`);
the Windows app draws the same vectors and colours from this page. The core's `ProxyIcon` says which state to show;
the hosts only draw it.

| State    | Shield                    | Logo                                 | Meaning                                  |
|----------|---------------------------|--------------------------------------|------------------------------------------|
| `NoKey`  | grey outline              | grey (the same lighter grey)         | no key installed                         |
| `Off`    | green outline             | dark grey (light grey in dark mode)  | a key is installed, the proxy is not used |
| `On`     | solid green               | white                                | the proxy is in use                      |
| `Slow`   | solid orange              | white                                | in use, the connection is slow           |
| `Failed` | solid red                 | white                                | in use, it failed                        |

## Geometry

Everything is in a **24 × 24 box, y pointing down** (as SVG and XAML). Scale it to the icon's size: 16 px/pt in rows
and menus, 24, and 28–32 at a page's title. The path data uses only absolute `M`, `L`, `C` and `Z`, one letter per
segment (no implicit repeats), with numbers separated by spaces and commas, which SVG's `d`, XAML's path markup and Win2D's `CanvasGeometry` (through
`CanvasPathBuilder`: `BeginFigure`, `AddLine`, `AddCubicBezier`, `EndFigure(Closed)`) all read. These strings are the
ones the Mac code draws (`ProxyShieldDrawing.shieldPath` and `logoPath`); change both together.

**Shield** (one closed figure, its centre line):

```
M 10.6,2.7 C 11.5,2.35 12.5,2.35 13.4,2.7 L 19.3,4.55 C 20.2,4.85 20.6,5.2 20.6,5.9 L 20.6,11 C 20.6,15.9 17,19.3 13.1,21.25 C 12.4,21.65 11.6,21.65 10.9,21.25 C 7,19.3 3.4,15.9 3.4,11 L 3.4,5.9 C 3.4,5.2 3.8,4.85 4.7,4.55 Z
```

- A peaked top (rounded at the peak by a cubic from 10.6 to 13.4), rounded top corners (cubics of about 1.3 units),
  straight sides from y 5.9 to 11, curving to a soft point at the bottom (a cubic of 2.2 units across the tip).
- **Line width 2**, round joins and caps. Outer edge: x 2.4 to 21.6, y about 1.4 to 22.6 (centred in the box).
- **Outline** (`NoKey`, `Off`): the path stroked. **Solid** (`On`, `Slow`, `Failed`): the path filled *and* stroked
  with the same colour, so outline and solid shields have the same edge.

**Logo**: three open lines and a circle, all stroked, never filled:

```
M 6.7,11 L 8.95,11 M 12.85,11 L 17.1,11 M 14.8,8.7 L 17.1,11 L 14.8,13.3
```

- Ring: centre **(10.9, 11)**, radius **1.95** (a circle; or, as path data, four cubics:
  `M 12.85,11 C 12.85,12.077 11.977,12.95 10.9,12.95 C 9.823,12.95 8.95,12.077 8.95,11 C 8.95,9.923 9.823,9.05 10.9,9.05 C 11.977,9.05 12.85,9.923 12.85,11 Z`).
- **Line width 1.9**, round caps and joins, lines and ring alike. The lines end on the ring's centre line, so they
  join it without a gap.

At 16 pt the shield's line is 1.33 pt and the logo's 1.27 pt: the weight of SF Symbols' regular `shield` at that size
and of the app's own marks (the menu bar arrows are 1.6 at 18).

### XAML (WinUI 3)

```xml
<Viewbox Width="16" Height="16">
  <Canvas Width="24" Height="24">
    <!-- Solid states: Fill and Stroke the state colour. Outline states: Stroke only. -->
    <Path Fill="{x:Bind ShieldFill}" Stroke="{x:Bind ShieldBrush}" StrokeThickness="2"
          StrokeLineJoin="Round" StrokeStartLineCap="Round" StrokeEndLineCap="Round"
          Data="M 10.6,2.7 C 11.5,2.35 12.5,2.35 13.4,2.7 L 19.3,4.55 C 20.2,4.85 20.6,5.2 20.6,5.9 L 20.6,11 C 20.6,15.9 17,19.3 13.1,21.25 C 12.4,21.65 11.6,21.65 10.9,21.25 C 7,19.3 3.4,15.9 3.4,11 L 3.4,5.9 C 3.4,5.2 3.8,4.85 4.7,4.55 Z"/>
    <Path Stroke="{x:Bind LogoBrush}" StrokeThickness="1.9"
          StrokeLineJoin="Round" StrokeStartLineCap="Round" StrokeEndLineCap="Round"
          Data="M 6.7,11 L 8.95,11 M 12.85,11 L 17.1,11 M 14.8,8.7 L 17.1,11 L 14.8,13.3"/>
    <Path Stroke="{x:Bind LogoBrush}" StrokeThickness="1.9">
      <Path.Data><EllipseGeometry Center="10.9,11" RadiusX="1.95" RadiusY="1.95"/></Path.Data>
    </Path>
  </Canvas>
</Viewbox>
```

### SVG (for reference renders)

```xml
<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" stroke-linecap="round" stroke-linejoin="round">
  <path d="M 10.6,2.7 C 11.5,2.35 12.5,2.35 13.4,2.7 L 19.3,4.55 C 20.2,4.85 20.6,5.2 20.6,5.9 L 20.6,11 C 20.6,15.9 17,19.3 13.1,21.25 C 12.4,21.65 11.6,21.65 10.9,21.25 C 7,19.3 3.4,15.9 3.4,11 L 3.4,5.9 C 3.4,5.2 3.8,4.85 4.7,4.55 Z"
        fill="#34C759" stroke="#34C759" stroke-width="2"/>
  <path d="M 6.7,11 L 8.95,11 M 12.85,11 L 17.1,11 M 14.8,8.7 L 17.1,11 L 14.8,13.3" fill="none" stroke="#FFFFFF" stroke-width="1.9"/>
  <circle cx="10.9" cy="11" r="1.95" fill="none" stroke="#FFFFFF" stroke-width="1.9"/>
</svg>
```

(That is `On` in light mode; for an outline state, `fill="none"` on the shield.)

## Colours

The Mac uses system colours, which macOS resolves at draw time in the light or dark appearance and with Increase
Contrast; the one colour of its own (the `Off` logo) has the same four variants. The values below were resolved on
this Mac (macOS 27, sRGB). Opaque colours only, so each is one hex value.

| State    | Part   | Mac colour              | Light     | Dark      |
|----------|--------|-------------------------|-----------|-----------|
| `NoKey`  | shield | `systemGray`            | `#8E8E93` | `#98989D` |
| `NoKey`  | logo   | `systemGray`            | `#8E8E93` | `#98989D` |
| `Off`    | shield | `systemGreen`           | `#34C759` | `#30D158` |
| `Off`    | logo   | own grey ("dark grey")  | `#48484A` | `#C7C7CC` |
| `On`     | shield | `systemGreen`           | `#34C759` | `#30D158` |
| `On`     | logo   | white                   | `#FFFFFF` | `#FFFFFF` |
| `Slow`   | shield | `systemOrange`          | `#FF8D28` | `#FF9230` |
| `Slow`   | logo   | white                   | `#FFFFFF` | `#FFFFFF` |
| `Failed` | shield | `systemRed`             | `#FF383C` | `#FF4245` |
| `Failed` | logo   | white                   | `#FFFFFF` | `#FFFFFF` |

The `Off` logo is "dark grey" in light mode and a light grey in dark mode: dark grey on a dark window would vanish
(1.8:1 on `#1E1E1E`). `NoKey`'s grey is lighter than the `Off` logo in light mode and dimmer in dark mode, so it reads
as the quiet, not-set-up state.

**Increase Contrast.** macOS swaps the system colours for their high-contrast variants by itself when the user turns
Increase Contrast on (Accessibility › Display); the app does nothing for them. They cannot be resolved off screen to
print here: `NSAppearance(named: .accessibilityHighContrastAqua)` gives the plain `Aqua` appearance (checked on this
Mac), so the high-contrast values exist only while the setting is on. The app's own `Off` logo grey has explicit
high-contrast values: **`#1C1C1E`** (light) and **`#E5E5EA`** (dark).

**Windows.** Light and dark mode take the table above (the app's theme, not the accent colour). Windows' counterpart of
Increase Contrast is a contrast theme (`AccessibilitySettings.HighContrast`), where apps draw with the theme's system
colours instead of their own: outline states (`NoKey`, `Off`) in `SystemColorWindowTextColor` (shield and logo), solid
states (`On`, `Slow`, `Failed`) filled with `SystemColorHighlightColor` and their logo in
`SystemColorHighlightTextColor`. The state then reads from the shape (outline or solid) and from the text beside the
icon, as Windows expects in a contrast theme.

## Hover (the user's choice, 2026-11-05)

The shield is a plain, borderless icon wherever it can be clicked (the menu bar menu's last row, Settings › Network,
the window's header): **no button chrome** in any state, no background, border or bezel, the hover included. On hover
it glows softly in its own colour, the shield colour of the table above (grey for `NoKey`, green for `Off` and `On`,
orange for `Slow`, red for `Failed`); there is no second colour table (`ProxyShieldDrawing.glowColor`).

| | Value |
|---|---|
| Glow | a shadow of the shield's shape in its colour, no offset, blur radius 3.5 pt at 16 pt (scaled with the size) |
| Opacity | 0.75 while hovered; 1 while pressed (a little brighter) |
| Fade | 0.15 s in and out |
| Hit target | unchanged: the shield's square in the row |

**Windows:** the same with no button chrome (a `Button` restyled to show the icon only, or the icon in a plain
element with pointer handlers): on `PointerEntered`, a `DropShadow` (Composition, `BlurRadius` 3.5 × scale, `Offset`
0, `Color` the state's shield colour, `Opacity` animated to 0.75 over 150 ms; 1 while pressed) on the icon's visual;
on `PointerExited` back to 0. `ThemeShadow` is grey only: use the Composition `DropShadow` (or an
`AttachedCardShadow`-like helper that takes a colour). In a contrast theme no glow (the focus visual shows instead).

## Contrast

`app/Tests/SeedwellAppTests/ProxyShieldTests.swift` checks the rendered pixels in light and dark mode: every state
draws colours of its own with its hue (grey, green, orange, red; solid only when in use); the logo stands out from what
is behind it by 3:1 inside an outline (WCAG's non-text contrast; the `Off` logo is about 8:1) and by at least 1.8:1 in
white on a solid shield (about 2:1 on dark mode's green, as macOS's own white-on-green marks; red is 3.6:1); and every
shield stands out from the window by 2:1.

## Renders

`Seedwell --render-snapshots <folder>` (with `SEEDWELL_RENDER_ONLY=proxy-shield` for these alone) writes:

- `proxy-shield-sheet.png`: the contact sheet, a row per state, 16, 24 and 32 pt on the light and the dark window,
  each drawn at 2x and magnified 4x without smoothing;
- `proxy-shield-<state>-<light|dark>-<16|24|32>.png`: each of those at 2x;
- `proxy-shield-rows-<light|dark>.png`: the icon in place, through the SwiftUI view, beside a title at 28 pt and in
  rows of body text at 16 pt.

The renders of the first version (2026-11-05) were handed to the coordinator in
`/private/tmp/claude-501/-Volumes-EksternalHome-Koding-Rust-TorrentRS/ac1d7bed-0e70-4c3f-9703-a94b88c60b86/scratchpad/proxy-icon/`
(a session's scratch folder: render them anew with the command above when it is gone).
