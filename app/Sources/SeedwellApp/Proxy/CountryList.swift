import AppKit
import SwiftUI

/// The torrent proxy's country list (SPEC §7H), the same in the shield's right-click menu and the "SOCKS5 proxy"
/// window: the core's countries ("No preference" first) in a list `visibleRows` rows tall (the model's
/// `country_visible_rows`; the rest scroll), a checkmark on the chosen one, scrolled so it shows when the list opens.
/// A click or Return chooses (`choose`: the owner closes the list and calls `proxy_set_country` off the main thread),
/// the arrows move, a letter jumps to the next country with it, Escape closes it.
@MainActor @Observable final class CountryListState {
    let countries: [ProxyCountry]
    /// The chosen code ("" none).
    let chosen: String
    let visibleRows: Int
    /// The row under the pointer or the arrows (nil: none yet, as in a menu that just opened).
    var highlighted: Int?
    @ObservationIgnored private let onChoose: @MainActor (String) -> Void
    @ObservationIgnored private let onCancel: @MainActor () -> Void

    init(countries: [ProxyCountry], chosen: String, visibleRows: UInt32,
         choose: @escaping @MainActor (String) -> Void, cancel: @escaping @MainActor () -> Void) {
        self.countries = countries
        self.chosen = chosen
        self.visibleRows = max(1, Int(visibleRows))
        onChoose = choose
        onCancel = cancel
    }

    /// The chosen country's row (No preference's when the code is not listed).
    var chosenIndex: Int { countries.firstIndex { $0.code == chosen } ?? 0 }

    /// Up (-1) or Down (+1): from the highlighted row, else from the chosen one.
    func move(_ by: Int) {
        guard !countries.isEmpty else { return }
        let from = highlighted ?? chosenIndex
        highlighted = highlighted == nil ? from : min(countries.count - 1, max(0, from + by))
    }

    /// Return: the highlighted row (none highlighted: the list closes and nothing changes).
    func chooseHighlighted() {
        if let i = highlighted { choose(at: i) } else { onCancel() }
    }

    /// A click on a row.
    func choose(at i: Int) {
        guard countries.indices.contains(i) else { return }
        onChoose(countries[i].code)
    }

    func cancel() { onCancel() }

    /// A letter typed: the first country after the highlighted one whose name starts with it (from the top again when
    /// none follows). Returns whether one does.
    @discardableResult func jump(to typed: String) -> Bool {
        guard let letter = typed.first.map(String.init), !countries.isEmpty else { return false }
        let start = (highlighted ?? -1) + 1
        let order = Array(countries.indices[min(start, countries.count)...]) + Array(countries.indices[..<min(start, countries.count)])
        guard let i = order.first(where: { countries[$0].name.prefix(1).localizedCaseInsensitiveCompare(letter) == .orderedSame }) else { return false }
        highlighted = i
        return true
    }
}

/// The list as drawn: menu rows (the menu's height and font, its rounded highlight) in a scroll view exactly
/// `visibleRows` rows tall, with a scrollbar that always shows (an overlay scroller shows only while scrolling, and
/// the user asked for one that says there is more).
struct CountryListView: View {
    let state: CountryListState
    @FocusState private var focused: Bool
    @State private var position = ScrollPosition(idType: Int.self)
    /// How far the list is scrolled, in points from the top.
    @State private var offset: CGFloat = 0
    /// Where the scrollbar's drag started (the offset then).
    @State private var dragFrom: CGFloat?

    /// A plain menu row's height, as the menu bar menu's rows.
    static var rowHeight: CGFloat { FooterRow.height }
    static var font: NSFont { Theme.NS.menu }
    /// The checkmark's column and the row's insets, as a menu draws them.
    static let checkWidth: CGFloat = 20
    static let inset: CGFloat = 8
    /// The scrollbar's track along the right edge: its width, and the knob's.
    static let track: CGFloat = 12
    static let knob: CGFloat = 6

    /// Wide enough for the longest name, with room for the scrollbar.
    static func width(_ countries: [ProxyCountry]) -> CGFloat {
        let longest = countries.map { ($0.name as NSString).size(withAttributes: [.font: font]).width }.max() ?? 0
        return ceil(inset + checkWidth + longest + 16 + track)
    }

    /// How tall the list is: the model's rows, never all of them.
    static func height(rows: Int) -> CGFloat { CGFloat(rows) * rowHeight }

    /// The first row shown when the list opens: the chosen one third from the top (whole rows, none cut).
    static func firstShown(chosen: Int, count: Int, rows: Int) -> Int { max(0, min(chosen - 2, count - rows)) }

    private var viewHeight: CGFloat { Self.height(rows: state.visibleRows) }
    private var contentHeight: CGFloat { CGFloat(state.countries.count) * Self.rowHeight }
    private var maxOffset: CGFloat { max(0, contentHeight - viewHeight) }

    var body: some View {
        ScrollView(.vertical) {
            VStack(spacing: 0) {
                ForEach(state.countries.indices, id: \.self) { i in row(i).id(i) }
            }
            .scrollTargetLayout()
            // Room for the scrollbar: the highlight stops short of it.
            .padding(.trailing, Self.track)
        }
        .scrollIndicators(.hidden)
        .scrollPosition($position)
        .onScrollGeometryChange(for: CGFloat.self, of: { $0.contentOffset.y + $0.contentInsets.top }) { _, y in offset = y }
        .frame(width: Self.width(state.countries), height: viewHeight)
        .overlay(alignment: .topTrailing) { scrollbar }
        .focusable()
        .focusEffectDisabled()
        .focused($focused)
        .onKeyPress(keys: [.upArrow, .downArrow, .return, .escape]) { press in
            switch press.key {
            case .upArrow: state.move(-1)
            case .downArrow: state.move(1)
            case .return: state.chooseHighlighted()
            default: state.cancel()
            }
            return .handled
        }
        .onKeyPress(characters: .letters, phases: .down) { press in state.jump(to: press.characters) ? .handled : .ignored }
        .onChange(of: state.highlighted) { _, i in if let i { reveal(i) } }
        .onAppear {
            focused = true
            let first = Self.firstShown(chosen: state.chosenIndex, count: state.countries.count, rows: state.visibleRows)
            position.scrollTo(id: first, anchor: .top)
        }
        .accessibilityIdentifier("proxy-country-list")
    }

    /// The arrows' row into view, by whole rows.
    private func reveal(_ i: Int) {
        let top = Int((offset / Self.rowHeight).rounded())
        if i < top { position.scrollTo(id: i, anchor: .top) }
        else if i >= top + state.visibleRows { position.scrollTo(id: i, anchor: .bottom) }
    }

    /// The knob: its length the share of the list that shows, its place how far it is scrolled; dragging it scrolls.
    private var scrollbar: some View {
        let h = viewHeight
        let length = max(Self.rowHeight, h * h / max(contentHeight, h))
        let travel = h - length - 4
        let y = 2 + (maxOffset > 0 ? travel * min(1, max(0, offset / maxOffset)) : 0)
        return ZStack(alignment: .top) {
            Color.clear
            Capsule()
                .fill(Color.primary.opacity(0.28))
                .frame(width: Self.knob, height: length)
                .offset(y: y)
        }
        .frame(width: Self.track, height: h)
        .contentShape(Rectangle())
        .gesture(DragGesture(minimumDistance: 0).onChanged { g in
            let from = dragFrom ?? offset
            dragFrom = from
            guard travel > 0 else { return }
            position.scrollTo(y: min(maxOffset, max(0, from + g.translation.height * maxOffset / travel)))
        }.onEnded { _ in dragFrom = nil })
        .opacity(maxOffset > 0 ? 1 : 0)
        .accessibilityHidden(true)
    }

    private func row(_ i: Int) -> some View {
        let c = state.countries[i]
        let lit = state.highlighted == i
        return HStack(spacing: 0) {
            Image(systemName: "checkmark")
                .font(.system(size: 11, weight: .semibold))
                .frame(width: Self.checkWidth, alignment: .center)
                .opacity(c.code == state.chosen ? 1 : 0)
            Text(c.name).font(Font(Self.font)).lineLimit(1)
            Spacer(minLength: 0)
        }
        .foregroundStyle(lit ? Color(nsColor: .selectedMenuItemTextColor) : Color.primary)
        .padding(.leading, Self.inset - 5)
        .frame(height: Self.rowHeight)
        .background {
            if lit { RoundedRectangle(cornerRadius: 5).fill(Color(nsColor: .selectedContentBackgroundColor)) }
        }
        .contentShape(Rectangle())
        .onHover { inside in
            if inside { state.highlighted = i } else if state.highlighted == i { state.highlighted = nil }
        }
        .onTapGesture { state.choose(at: i) }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(c.name)
        .accessibilityAddTraits(c.code == state.chosen ? [.isButton, .isSelected] : .isButton)
    }
}

/// The list as a popover's content: the menu's 5-point inset around it.
struct CountryListPopover: View {
    let state: CountryListState
    var body: some View { CountryListView(state: state).padding(5) }
}
