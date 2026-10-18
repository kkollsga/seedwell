import SwiftUI

// MARK: Download Location of a favorite tracker (SPEC §7A.4, §7E.6)

extension ProfileRecord {
    /// The tracker sets a folder of its own, tracker-wide or for a content type.
    var hasOwnFolders: Bool {
        incompleteDir != nil || finishedDir != nil || categories.contains { $0.incompleteDir != nil || $0.finishedDir != nil }
    }

    /// Same folders (tracker-wide and per content type), whatever else differs.
    func sameFolders(as o: ProfileRecord) -> Bool {
        incompleteDir == o.incompleteDir && finishedDir == o.finishedDir
            && Category.all.allSatisfy { folders(for: $0) == o.folders(for: $0) }
    }

    mutating func copyFolders(from o: ProfileRecord) {
        incompleteDir = o.incompleteDir
        finishedDir = o.finishedDir
        categories = o.categories
    }

    /// The tracker's folders for one content type (both `nil` when it sets none).
    func folders(for category: Category) -> CategoryLocationSetting {
        categories.first { $0.category == category } ?? CategoryLocationSetting(category: category, incompleteDir: nil, finishedDir: nil)
    }

    /// Sets (or, with two `nil`s, removes) the tracker's folders for a content type.
    mutating func setFolders(_ s: CategoryLocationSetting) {
        categories.removeAll { $0.category == s.category }
        if s.incompleteDir != nil || s.finishedDir != nil { categories.append(s) }
    }
}

/// Where a favorite's torrents are written and kept: its own Incomplete and Finished folders, and
/// optionally some for a content type. Empty = "Same as Settings". The same view serves the favorite's
/// Info page and Settings → Rules → Trackers, both editing a `ProfileRecord`.
struct TrackerLocationEditor: View {
    @Binding var record: ProfileRecord
    /// Different-disk warnings for the pairs this tracker sets (`category == nil`: the tracker-wide pair).
    var warnings: [VolumeWarning]
    @State private var showTypes = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            row("Incomplete", $record.incompleteDir)
            row("Finished", $record.finishedDir)
            if warnings.contains(where: { $0.category == nil }) { DifferentDisksWarning().padding(.horizontal, -14) }
            Text("Torrents of this tracker use these folders instead of the ones in Settings → Folders; a rule's own Finished folder still comes first.")
                .font(.system(size: 11)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            DisclosureGroup(isExpanded: $showTypes) {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(Category.all, id: \.self) { cat in category(cat) }
                }.padding(.top, 4)
            } label: {
                Text("By content type").font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
        .onAppear { showTypes = !record.categories.isEmpty }
    }

    private func row(_ label: String, _ path: Binding<String?>) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Text(label).font(.system(size: 12)).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
            OptionalPathField(path: path, placeholder: "Same as Settings", width: 220)
        }
    }

    private func category(_ cat: Category) -> some View {
        let binding = Binding<CategoryLocationSetting>(get: { record.folders(for: cat) }, set: { record.setFolders($0) })
        let own = record.folders(for: cat)
        return VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Image(systemName: cat.symbol).frame(width: 18).foregroundStyle(.secondary)
                Text(cat.label).font(.system(size: 12, weight: .semibold))
                Spacer(minLength: 8)
                if own.incompleteDir != nil || own.finishedDir != nil {
                    Text("custom folders").font(.system(size: 11)).foregroundStyle(Color.accentColor)
                }
                if warnings.contains(where: { $0.category == cat }) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Color.orange)
                        .help("On different disks: finishing a download copies the files")
                }
            }
            row("Incomplete", binding.incompleteDir)
            row("Finished", binding.finishedDir)
        }
        .padding(.vertical, 3)
    }
}
