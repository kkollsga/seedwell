import SwiftUI

/// The header line of a mail on its card (SPEC 7G.8): the core's banner (`ChatMailBanner`), drawn.
struct ChatMailBannerView: View {
    let banner: ChatMailBanner

    var body: some View {
        switch banner {
        case .note(let line, let seen):
            Label(line, systemImage: seen ? "eye" : "bell")
                .font(.system(size: 11.5)).foregroundStyle(.secondary)
        case .status(let title, let done):
            Label(title, systemImage: done ? "checkmark.circle.fill" : "clock")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(done ? Color.green : Color.orange)
        case .mail(let chip, let tint, let to, let re, let summary):
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if let chip {
                        Text(chip).font(.system(size: 10.5, weight: .semibold))
                            .padding(.horizontal, 6).padding(.vertical, 1.5)
                            .background(Capsule().fill(Self.color(tint).opacity(0.18)))
                            .foregroundStyle(Self.color(tint))
                    }
                    if let to {
                        Label(to, systemImage: "tray.and.arrow.down").font(.system(size: 11.5)).foregroundStyle(.secondary)
                    }
                    if let re {
                        Text(re).font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary)
                    }
                }
                if let summary {
                    Text(summary).font(.system(size: 13.5, weight: .semibold)).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    static func color(_ tint: ChatMailTint) -> Color {
        switch tint {
        case .red: .red
        case .blue: .blue
        case .orange: .orange
        case .teal: .teal
        case .neutral: .secondary
        }
    }
}
