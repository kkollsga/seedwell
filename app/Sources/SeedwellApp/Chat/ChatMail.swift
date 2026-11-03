import SwiftUI

/// The header line of a mail on its card (SPEC 7G.8): the core's banner (`ChatMailBanner`), drawn.
struct ChatMailBannerView: View {
    let banner: ChatMailBanner

    var body: some View {
        switch banner {
        case .note(let line, let seen):
            Label(line, systemImage: seen ? "eye" : "bell")
                .font(Theme.caption).foregroundStyle(.secondary)
        case .status(let title, let done):
            Label(title, systemImage: done ? "checkmark.circle.fill" : "clock")
                .font(Theme.secondary.weight(.semibold))
                .foregroundStyle(done ? Color.green : Color.orange)
        case .mail(let chip, let tint, let to, let re, let summary):
            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                HStack(spacing: Theme.Space.s - 2) {
                    if let chip {
                        Text(chip).font(Theme.micro.weight(.semibold))
                            .padding(.horizontal, 6).padding(.vertical, 1.5)
                            .background(Capsule().fill(Self.color(tint).opacity(0.18)))
                            .foregroundStyle(Self.color(tint))
                    }
                    if let to {
                        Label(to, systemImage: "tray.and.arrow.down").font(Theme.caption).foregroundStyle(.secondary)
                    }
                    if let re {
                        Text(re).font(Theme.monoSmall).foregroundStyle(.secondary)
                    }
                }
                if let summary {
                    Text(summary).font(Theme.headline).fixedSize(horizontal: false, vertical: true)
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
