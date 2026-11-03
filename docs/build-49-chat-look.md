# Build 49: the chat's new look (for the Windows app)

Branch `build-49-chat-look`. The Mac app's chat was redesigned with the user (2026-11-02): less text, one type
scale, no reflow on hover, the reply bar at the bottom, "ours" vs "theirs", bubbles in private conversations, run
metadata under the run, images at the top (galleries), and image references in the text. Everything that is a rule
or a word moved into Rust (`crates/ffi/src/chat_page.rs`, `chat_markdown.rs`, `chat_images.rs`, `chat_text.rs`,
`library.rs`), so `app-windows/ChatPage.cs` gets it by drawing the new model. SPEC §7G.7 ("The look", images, image
references) and `crates/sync-proto/PROTOCOL.md` (image references) say the same in full. The Mac's renders are in the
coordinator's report.

## 1. Model changes (what breaks the build, and what to draw)

### `ChatMessageCard`

| Was | Now |
|---|---|
| `author: Option<ChatMessageAuthor>` (on the **first** card of a run; `name`, `badge`, `computer`, `time`) | `meta: Option<ChatMessageMeta>` on the **last** card of a run only: `name`, `detail` ("Kari's iPhone · 14.32", or just "14.32" on this device), `initial` ("K"), `help` (tooltip: "Seedwell, an agent on Studio iMac"; may be empty) |
| (none) | `starts_run: bool`: the first card of a run (16 px above it; 0 inside a run) |
| `mine: bool` | `who: ChatAuthorKind` = `Mine` · `OurAgent` · `Theirs` · `TheirAgent` (the user's devices from Settings › Devices count as "mine") |
| (none) | `time: String`: every message's time, for the hover pill |
| `reply_quote: Option<String>` ("↳ Kari (Studio iMac): …") | `reply_quote: Option<ChatQuote>` (`title` "Kari", `text` one line) |
| `replying: String` ("Replying to Kari (Studio iMac): …") | `replying: ChatQuote` (`title` "Replying to Kari", `text`) |
| (none) | `gallery: bool`: more than one image (draw a gallery; order = `images`) |
| (none) | `palette: Option<u32>`: another person's colour index (0–7) in this channel; `None` for the user's and their agents' |

`ChatQuote` also has `message_id` (the quoted card, to scroll to), `who` and `palette` (the quoted author's edge).
`chat_palette_size()` is 8; `chat_code_copy_help()` is "Copy".

`ChatMessageAuthor` and its `badge`/`computer` are gone. Draw `meta` as one small quiet line directly **under** the
run's last message, **outside** its block or bubble, aligned with its edge (right edge for a right-hand bubble):
`name` + " · " + `detail` in Caption (12 epx), regular weight, the tertiary text colour; before it a 13 px outlined
round initial (`Theirs`), the same circle with the agent symbol (`TheirAgent`), or the bare agent symbol
(`OurAgent`), all in the same tertiary colour; nothing for `Mine`. Its tooltip is `help`. 18 px tall, 4 px under the
message.

### `ChatPageModel` / `ChatPageRead`

- `ChatPageModel.bubbles: bool`: true for a private conversation (draw bubbles), false for a channel (one column).
- `ChatPageRead.own_devices: Vec<String>` and `ChatPageRead.joined: Vec<SyncJoined>` (uniffi defaults `[]`): filled by
  `Core::chat_page_read`; pass them through. `SyncJoined` = `device_id`, `at_s` (first grant), `user` (profile).

**The colour rule (Rust, `palette_map`):** in each channel, people (a user's devices together by their profile's user
name, else a device alone) take indices 0, 1, 2, … in the order they joined the channel (each device's earliest grant
in the membership, ties by device id) and keep them for good: one who left stays in the list (the membership keeps
their grant), so nobody else's index changes, and a later joiner takes the next. The user counts as a person too (their
own messages show the accent), so every device agrees on everyone's index. Up to 8 people never share a colour; the
9th takes index 0 again. A person's agents carry the person's index (drawn muted). A device the channel does not list
(a private conversation) gets an index from its id (FNV-1a mod 8). The host only maps index → colour.

### `ChatRun` (Markdown)

- `image: Option<u32>` (uniffi default `None`): a reference to the message's image at that index (0 = first). Draw
  as a chip where it stands (photo glyph + the run's text, accent colour; white and underlined on an accent bubble).
  Click → the gallery shows that image (`selection = index`) and the message's top scrolls into view.

### New functions

- `chat_image_marker(index) -> String`: `![Image 2](att:2)` for index 1.
- `chat_composer_with_images(draft, first, count, at) -> Option<ChatComposerEdit>`: the draft with the markers of
  attachments `first..first+count` where they went, and the caret after them (`at`, `caret`: UTF-16, the text box's
  `SelectionStart`/`SelectionLength`). Call it when images are **pasted into a field that has text** (`at` = the
  selection) or **dropped onto its text** (`at` = the character under the drop point, length 0). `None`: an empty
  field, no marker. The attach button, a drop elsewhere and `--attach` add no marker.
- `chat_composer_image_refs(draft) -> Vec<ChatTextRange>`: the markers in the draft (UTF-16), drawn in the accent
  colour on a faint tint of it in the composer.
- `chat_composer_without_image(draft, index) -> String`: call when a chip is removed from the composer; it drops that
  image's markers and renumbers the later ones.
- `chat_gallery_counter(active, count) -> String`: "2 / 5" (active counts from 1).
- `chat_image_layout_samples(max_side)`: fixtures for snapshots (wide, tall, small 120×80, icon 64×32, gallery-1…5).

### Words that changed (no code change needed, they come from Rust)

- Channel line: "2 of 3 online · Muted · shared until 1 Nov" (no role, no "agents: yes"). Topic line: its channel's.
  Private: "End-to-end encrypted" / "Not connected".
- Toolbar: Members help "Members"; Share help "Share: invite another PC to this channel"; ⋯ help "More". Tag menu
  help "Tag". Reply-cancel help "Cancel Reply (Esc)".
- Empty: "No messages yet" (both kinds). Read-only composer: "Read only". Private, not connected:
  "You can write when Kari's iPhone is connected directly".

## 2. Type and spacing scale (Mac → Windows)

One file on the Mac: `app/Sources/SeedwellApp/Support/Theme.swift`. Suggested Windows mapping (Segoe UI Variable,
Fluent ramp; the Mac's 13 pt body is Windows' 14 epx body):

| Style | Mac (SF Pro) | Windows (Segoe UI Variable) | Used for |
|---|---|---|---|
| title | 17 semibold | 20 Semibold (Subtitle) | page name in the title bar |
| heading | 15 semibold | 16 Semibold | sheet titles, Markdown h1 |
| headline | 13 semibold | 14 Semibold (Body Strong) | names, row titles, Markdown h2–h3 |
| body | 13 regular | 14 Regular (Body) | messages, rows, controls |
| secondary | 12 regular | 13 Regular | subtitles, notes, tables |
| caption | 11 regular | 12 Regular (Caption) | times, tags, meta line, quotes |
| micro | 10 medium | 11 Semibold | capsules, counts, code language |
| mono | SF Mono 12 | Cascadia Mono 13 | code blocks, JSON |
| monoSmall | SF Mono 11 | Cascadia Mono 12 | a mail's `re` |

Line spacing: body text +3 (13 on ~18), secondary +2. Spacing on a 4 grid: 2, 4, 8, 12, 16, 24, 32. Radii: 6
(chips, run blocks), 8 (code, file cards), 12 (composer, images 10), bubbles 16. Icons: 11 / 13 / 17 / 40, Send 22.

## 3. Behaviour to mirror

- **Hover never reflows.** The time · Tag (menu) · Reply show, small and quiet (caption, tertiary time, secondary
  icons, on a faint bar capsule), in the row **under** the message:
  - under a run's last message: at the right end of the meta line's row (channel), or beside the meta line on the
    side away from the bubble's edge (private), with its room kept while hidden (opacity 0), so nothing moves;
  - under a message inside a run: in the same place, drawn as an overlay over the boundary to the next message
    (channel: centred on the edge; private: hanging under the bubble's far end). It never pushes the next message
    down.
- **Every message has a left edge** (2.5 px): `Mine` the accent (and only `Mine` has the 7 % accent tint),
  `OurAgent` the muted accent, `Theirs` the person's palette colour, `TheirAgent` the same hue at about half the
  saturation. The Mac's palette (HSB, hue in degrees, by index): 28, 44, 62, 168, 262, 288, 318, 342; saturation
  0.42 light / 0.36 dark, brightness 0.68 light / 0.78 dark (agents: saturation × 0.45). Mirror these on Windows.
- **Reply quotes**: an inset block (6 px radius, 5 % fill; on an accent bubble 16 % white): the quoted author's
  edge colour (white for the user's own on an accent bubble), the name in caption secondary, one line of text in
  caption. Click: scroll the original to the centre and light it (accent 16 % behind it, ~1.4 s).
- **Code blocks**: a copy button (`doc.on.doc` / Segoe Fluent "Copy" glyph) in the top-right corner, shown on hover or
  keyboard focus, over the block; copies the code exactly; a check mark for 1.5 s. Tooltip and name from
  `chat_code_copy_help()`.
- **In-run spacing**: 0, the user's choice (2026-11-02): a run's messages touch and form one block with one edge,
  and the hover controls of a message inside it sit on the boundary to the next. One constant on the Mac
  (`Theme.inRunGap`).
- **Channel column**: centred, at most 820 wide. A run's messages touch as one block (6 px radius only at the run's
  ends). `Mine`: accent at 7 % behind it (11 % on hover). Everyone else: no background; 4.5 % grey on hover. The
  muted accent of `OurAgent` is the accent blended ~65 % with grey.
- **Private conversation**: bubbles (radius 16, padding 12×7), max 520 wide; `Mine` right in the accent (white text),
  `OurAgent` right in the muted accent, others left in 8 % grey. An image- or file-only message has no bubble. Meta
  line under the run's last bubble, on its side.
- **Reply**: Reply sets the model's reply (`Mac: LibraryModel.reply(to:)`); a bar docked on top of the composer,
  inside the same rounded field: 3 px accent bar, "Replying to Kari" (caption semibold, accent), one line of text
  (caption, secondary), × at the right. Focus goes to the field. Esc in the field cancels the reply (and is passed
  on when there is none); × cancels; changing conversation cancels. Sending clears it.
- **Composer**: one rounded field (radius 12, 4.5 % fill, 10 % border, accent border while focused): paperclip
  icon, the text field (plain, 1–8 lines), Send as a filled arrow-up circle (accent when it can send, grey when not),
  tooltips only, no captions.
- **Toolbar**: agents mark (the agent symbol, tooltip = the tag's help), Members (people icon), Share (share icon),
  ⋯; titles become accessibility names.
- **Images at the top of the message**, above its text; file cards stay compact under the text.
  - One image: at most the content width and 420 high, never larger than its own pixels / scale (no upscaling),
    centred (on its side under a bubble), radius 10, no caption (name and size in the tooltip). Click opens it.
  - Gallery (Finder's Gallery view): stage as tall as the tallest image fitted (so switching never changes the
    height), the active image centred in it; filmstrip under it: 46 px square thumbnails (picture fitted with 5 px
    inset), 4 px apart, centred, scrolling sideways when needed; the active one with an accent rounded highlight (22 %
    fill + 2 px accent border, radius 7), the others at 60 % opacity until hovered; ‹ › round buttons over the stage's
    edges while the pointer is on it; Left/Right keys while focused; "2 / 5" (`chat_gallery_counter`) as a quiet
    capsule at the stage's top-right. The active index is view state.
- **Image references**: see `ChatRun.image` above; a reference to an image the message does not carry arrives as
  plain text already.
