import AppKit
import SwiftUI

/// The app's one type and spacing scale (drawing only): SF Pro, the system font, in a few sizes and weights, SF Mono for
/// code, and spacing on a 4-point grid. Every view takes its fonts and spaces from here; nothing sets a size of its own.
/// docs/build-49-chat-look.md maps it to Segoe UI Variable for the Windows app.
///
/// | Style       | Size | Weight   | For                                                              |
/// |-------------|------|----------|------------------------------------------------------------------|
/// | `title`     | 17   | semibold | a page's name in the title bar                                   |
/// | `heading`   | 15   | semibold | a section or a sheet's title, a Markdown heading                 |
/// | `headline`  | 13   | semibold | names, a row's title, labels that lead                           |
/// | `body`      | 13   | regular  | messages, rows, fields and controls (macOS's own body size)      |
/// | `secondary` | 12   | regular  | the line under a row, notes, summaries                           |
/// | `caption`   | 11   | regular  | times, tags, footnotes, the quiet line under a field             |
/// | `micro`     | 10   | medium   | capsules, badges and counts                                      |
/// | `mono`      | 12   | regular  | code, JSON, ids (SF Mono)                                        |
/// | `monoSmall` | 11   | regular  | small code: a mail's `re`, a path in a note (SF Mono)            |
enum Theme {
    enum Size {
        static let title: CGFloat = 17
        static let heading: CGFloat = 15
        static let body: CGFloat = 13
        static let secondary: CGFloat = 12
        static let caption: CGFloat = 11
        static let micro: CGFloat = 10
        static let mono: CGFloat = 12
        static let monoSmall: CGFloat = 11
    }

    /// The text's font family: the one switch. SF Pro (`.system`) is the app's (the user's choice of 2026-11-02, after
    /// comparing it with Inter). `.inter` stays as the one-line way to another family: it is used only when the launch
    /// environment asks for it (`SEEDWELL_FONT=Inter`) and the app carries the font (`Resources/Fonts/InterVariable.ttf`;
    /// none is bundled now, so the app stays on SF Pro). Code stays SF Mono either way.
    enum Family: String {
        case system
        case inter = "Inter"
    }

    static let family: Family = {
        guard ProcessInfo.processInfo.environment["SEEDWELL_FONT"] == Family.inter.rawValue,
              let url = Bundle.main.resourceURL?.appendingPathComponent("Fonts/InterVariable.ttf"),
              CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil) else { return .system }
        return .inter
    }()

    /// Text at `size` in the family's face.
    static func font(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        switch family {
        case .system: .system(size: size, weight: weight)
        case .inter: .custom("Inter Variable", fixedSize: size).weight(weight)
        }
    }

    static let title = font(Size.title, .semibold)
    static let heading = font(Size.heading, .semibold)
    static let headline = font(Size.body, .semibold)
    static let body = font(Size.body)
    static let secondary = font(Size.secondary)
    static let caption = font(Size.caption)
    static let micro = font(Size.micro, .medium)
    static let mono = Font.system(size: Size.mono, design: .monospaced)
    static let monoSmall = Font.system(size: Size.monoSmall, design: .monospaced)

    /// Extra space between the lines of running text (a message, a note of several lines): 13-point text on an
    /// 18-point line, 12 on 16.5.
    enum Line {
        static let body: CGFloat = 3
        static let secondary: CGFloat = 2
    }

    /// Spacing on a 4-point grid (2 for hairline gaps).
    enum Space {
        static let hair: CGFloat = 2
        static let xs: CGFloat = 4
        static let s: CGFloat = 8
        static let m: CGFloat = 12
        static let l: CGFloat = 16
        static let xl: CGFloat = 24
        static let xxl: CGFloat = 32
    }

    /// The extra space between the messages of one run in a conversation: 0, they touch (a channel's run is one
    /// block, and the pointer's controls of a message inside it are drawn over the edge to the next; the user's choice
    /// of 2026-11-02); 6 would set them a little apart, those controls in the gap.
    static let inRunGap: CGFloat = 0

    /// Corner radii: small (chips, code), medium (cards, fields, images), large (bubbles, panels).
    enum Radius {
        static let small: CGFloat = 6
        static let medium: CGFloat = 8
        static let large: CGFloat = 12
    }

    /// SF Symbols beside text take the text's style; these are for symbols that stand alone.
    enum Icon {
        /// In a line of caption text, a hover pill, a chip.
        static let small: CGFloat = 11
        /// A toolbar or row button.
        static let regular: CGFloat = 13
        /// A file's icon on its card, an avatar's symbol.
        static let large: CGFloat = 17
        /// The composer's Send (a filled circle with an arrow).
        static let send: CGFloat = 22
        /// The large icon of an empty page or a sheet.
        static let hero: CGFloat = 40
    }

    enum Sidebar {
        /// What the outline list sets a plain row's label in by, past the column of the disclosure triangles.
        static let disclosureWidth: CGFloat = 16
    }

    /// The same scale for AppKit (the menu bar menu, alerts): the menu's own font for its rows, and the sizes above.
    enum NS {
        static var menu: NSFont { .menuFont(ofSize: 0) }
        static var menuSmall: NSFont { .menuFont(ofSize: Size.caption) }
        static func body(_ weight: NSFont.Weight = .regular) -> NSFont { font(Size.body, weight) }
        static func secondary(_ weight: NSFont.Weight = .regular) -> NSFont { font(Size.secondary, weight) }
        static func caption(_ weight: NSFont.Weight = .regular) -> NSFont { font(Size.caption, weight) }
        static func micro(_ weight: NSFont.Weight = .medium) -> NSFont { font(Size.micro, weight) }

        private static func font(_ size: CGFloat, _ weight: NSFont.Weight) -> NSFont {
            let system = NSFont.systemFont(ofSize: size, weight: weight)
            guard family == .inter else { return system }
            let d = NSFontDescriptor(fontAttributes: [.family: "Inter Variable", .traits: [NSFontDescriptor.TraitKey.weight: weight]])
            return NSFont(descriptor: d, size: size) ?? system
        }
        static func digits(_ size: CGFloat = Size.caption) -> NSFont { .monospacedDigitSystemFont(ofSize: size, weight: .regular) }
    }
}
