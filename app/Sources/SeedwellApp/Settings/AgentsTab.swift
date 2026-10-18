import SwiftUI

// MARK: Agents (concept 9, SPEC §9A)

struct AgentsTab: View {
    @Bindable var model: SettingsModel
    @State private var copied = false
    @State private var cliPath: String?
    @State private var skillInstalled = false
    @State private var note: String?

    private let home = NSHomeDirectory()
    private var pathEnv: String { ProcessInfo.processInfo.environment["PATH"] ?? "" }
    private var bundled: String { CommandLineTool.bundledPath() }

    private var allowed: Binding<Bool> {
        Binding(get: { model.app.agents.permission != .off },
                set: { model.app.agents.permission = $0 ? .control : .off })
    }

    var body: some View {
        SettingsPage {
            SettingsGroup(title: "Agent access") {
                SettingsRow(label: "Allow agents", hint: "Local only. Never on the network.") {
                    Toggle("", isOn: allowed).labelsHidden().toggleStyle(.switch)
                        .help("Let AI agents on this Mac connect to Seedwell")
                }
                SettingsRow(label: "Permission", hint: "Full also deletes files and changes settings.") {
                    Picker("", selection: $model.app.agents.permission) {
                        Text("Read-only").tag(AgentPermission.readOnly)
                        Text("Control").tag(AgentPermission.control)
                        Text("Full").tag(AgentPermission.full)
                    }
                    .pickerStyle(.segmented).labelsHidden().fixedSize()
                    .disabled(model.app.agents.permission == .off)
                }
                SettingsRow(label: "Never available") {
                    Text("Client identity · tracker timing · passkeys · auth headers").foregroundStyle(.secondary)
                }
            }
            SettingsGroup(title: "Command line") {
                ActionRow(
                    title: "seedwell command",
                    text: cliPath.map { "Installed: \($0)" } ?? "Not installed. It is inside the app; you can also run it by its full path.",
                    mono: false
                ) {
                    Button(copied ? "Copied" : "Copy CLI Path") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(cliPath ?? bundled, forType: .string)
                        copied = true
                        Task { try? await Task.sleep(for: .seconds(1.5)); copied = false }
                    }
                    .help("Copy the path of the seedwell command to your clipboard")
                    if cliPath == nil {
                        Button("Install Command-Line Tool…") { installTool() }
                            .help("Link seedwell into /usr/local/bin or ~/.local/bin so you can type it in a terminal. Asks first.")
                    }
                }
                ActionRow(
                    title: "Skill for Claude Code",
                    text: skillInstalled
                        ? "Installed in ~/.claude/skills/seedwell."
                        : "Teaches Claude Code the seedwell commands. Writes two text files to ~/.claude/skills/seedwell, only when you press the button."
                ) {
                    if skillInstalled {
                        Button("Remove Skill") { removeSkill() }
                            .help("Delete the skill files from ~/.claude/skills/seedwell")
                    }
                    Button(skillInstalled ? "Update Skill" : "Install Skill for Claude Code") { installSkill() }
                        .help("Write the Seedwell skill to ~/.claude/skills/seedwell so Claude Code knows the commands")
                }
                Text(note ?? "The seedwell command lets you and AI agents control Seedwell from a terminal (seedwell list, seedwell add, seedwell help agents). It follows the permission above and nothing is sent anywhere.")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 14).padding(.vertical, 10)
            }
            SettingsGroup(title: "Activity") {
                if model.activity.isEmpty {
                    Text("Nothing yet. Every action an agent takes is listed here.")
                        .font(.system(size: 13)).foregroundStyle(.secondary).padding(14)
                }
                ForEach(Array(model.activity.prefix(8).enumerated()), id: \.offset) { _, a in
                    HStack(alignment: .firstTextBaseline, spacing: 16) {
                        Text(Self.time(a.atMs)).foregroundStyle(.secondary).monospacedDigit().frame(width: 60, alignment: .leading)
                        (Text(a.client).bold() + Text(" ") + Text(a.summary)
                            + (a.outcome == "ok" ? Text("") : Text(" (\(a.outcome))").foregroundStyle(.red)))
                            .lineLimit(2)
                        Spacer()
                    }
                    .font(.system(size: 13.5))
                    .padding(.horizontal, 14).padding(.vertical, 10)
                }
                SettingsRow(label: "Notify me") {
                    HStack(spacing: 10) {
                        Toggle("", isOn: $model.app.agents.notifyChanges).labelsHidden().toggleStyle(.switch)
                            .help("Show a notification whenever an agent changes something")
                        Text("when an agent changes something")
                    }
                }
                .foregroundStyle(.secondary)
            }
        }
        .onAppear {
            model.reloadActivity()
            refresh()
        }
    }

    private func refresh() {
        cliPath = CommandLineTool.findInstalled(home: home, pathEnv: pathEnv)
        skillInstalled = CommandLineTool.skillInstalled(home: home)
    }

    // MARK: Actions (each one is a button the user pressed; nothing here runs on its own)

    private func installTool() {
        let a = NSAlert()
        a.messageText = "Install the command-line tool?"
        a.informativeText = "This adds a link named “seedwell” that points into the Seedwell app, so you can type seedwell in a terminal.\n\n/usr/local/bin is shared and may ask for an administrator. ~/.local/bin is yours alone; add it to your PATH if it is not there."
        a.addButton(withTitle: "Link in ~/.local/bin")
        a.addButton(withTitle: "Link in /usr/local/bin")
        a.addButton(withTitle: "Cancel")
        let folders = CommandLineTool.installFolders(home: home)
        let dir: String
        switch a.runModal() {
        case .alertFirstButtonReturn: dir = folders[1]
        case .alertSecondButtonReturn: dir = folders[0]
        default: return
        }
        do {
            let link = try CommandLineTool.link(into: dir, target: bundled)
            let onPath = CommandLineTool.isOnPath(dir, pathEnv: pathEnv)
            note = onPath ? "Linked \(link). Open a new terminal window and type seedwell help." : "Linked \(link). Add \(dir) to your PATH to type seedwell without its full path."
        } catch CommandLineTool.LinkError.exists(let path) {
            note = "\(path) already exists and points somewhere else, so it was left alone."
        } catch {
            note = "Could not link into \(dir). In a terminal: sudo ln -s '\(bundled)' \(dir)/seedwell"
        }
        refresh()
    }

    private func installSkill() {
        let dir = CommandLineTool.skillFolder(home: home)
        Task { @MainActor in
            var r = await CommandLineTool.run(["skill", "install", "--dir", dir])
            if r.status != 0, r.err.contains("--force") {
                let a = NSAlert()
                a.messageText = "Replace the changed skill files?"
                a.informativeText = "Files in \(dir) were changed since Seedwell wrote them. Replacing them loses those changes."
                a.addButton(withTitle: "Replace")
                a.addButton(withTitle: "Cancel")
                guard a.runModal() == .alertFirstButtonReturn else { return }
                r = await CommandLineTool.run(["skill", "install", "--dir", dir, "--force"])
            }
            note = r.status == 0
                ? "Skill written to \(dir). Claude Code uses it in new sessions."
                : "Could not install the skill: \(r.err.trimmingCharacters(in: .whitespacesAndNewlines))"
            refresh()
        }
    }

    private func removeSkill() {
        let dir = CommandLineTool.skillFolder(home: home)
        Task { @MainActor in
            let r = await CommandLineTool.run(["skill", "uninstall", "--dir", dir])
            note = r.status == 0 ? "Skill removed from \(dir)." : "Could not remove the skill: \(r.err.trimmingCharacters(in: .whitespacesAndNewlines))"
            refresh()
        }
    }

    private static func time(_ ms: UInt64) -> String {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(ms) / 1000))
    }
}

/// A title and a line of text on the left, normal buttons on the right.
private struct ActionRow<Buttons: View>: View {
    let title: String
    let text: String
    var mono = false
    @ViewBuilder var buttons: Buttons

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.system(size: 13))
                Text(text).font(.system(size: 11.5, design: mono ? .monospaced : .default)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            HStack(spacing: 8) { buttons }.fixedSize()
        }
        .padding(.horizontal, 14).padding(.vertical, 9)
        .frame(minHeight: 40)
    }
}
