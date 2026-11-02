import SwiftUI

// MARK: Download Location of a favorite tracker (SPEC §7A.4, §7E.6)

/// Where a favorite's torrents are written and kept: its own Incomplete and Finished folders, and optionally some for a
/// content type. The core decides the rows, the words and what a change does (`trackerFoldersView`,
/// `trackerFoldersSet`); this draws them. The same view serves the favorite's Info page and Settings → Rules →
/// Trackers, both editing a `ProfileRecord`.
struct TrackerLocationEditor: View {
    @Binding var record: ProfileRecord
    /// Different-disk warnings for the pairs this tracker sets (`category == nil`: the tracker-wide pair).
    var warnings: [VolumeWarning]
    @State private var showTypes = false

    var body: some View {
        let v = trackerFoldersView(record: record, warnings: warnings)
        VStack(alignment: .leading, spacing: 6) {
            ForEach(v.rows, id: \.label) { row($0) }
            if let w = v.warning { DifferentDisksWarning(text: w).padding(.horizontal, -14) }
            Text(v.note)
                .font(.system(size: 11)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            DisclosureGroup(isExpanded: $showTypes) {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(v.types, id: \.category) { type($0) }
                }.padding(.top, 4)
            } label: {
                Text(v.typesTitle).font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
        .onAppear { showTypes = v.typesOpen }
    }

    private func row(_ r: TrackerFolderRow) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Text(r.label).font(.system(size: 12)).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
            OptionalPathField(row: r, width: 220) { path in
                record = trackerFoldersSet(record: record, category: r.category, field: r.field, path: path)
            }
        }
    }

    private func type(_ t: TrackerFolderType) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Image(systemName: t.category.symbol).frame(width: 18).foregroundStyle(.secondary)
                Text(t.title).font(.system(size: 12, weight: .semibold))
                Spacer(minLength: 8)
                if let c = t.custom {
                    Text(c).font(.system(size: 11)).foregroundStyle(Color.accentColor)
                }
                if let w = t.warning {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Color.orange).help(w)
                }
            }
            ForEach(t.rows, id: \.label) { row($0) }
        }
        .padding(.vertical, 3)
    }
}
