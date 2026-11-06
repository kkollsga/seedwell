import Foundation
import MachO
import Testing

/// How the app extensions' executables start, read from the bundle `app/build.sh` built last (it runs before these
/// tests, locally in `tools/check.sh` and on CI). The system starts an extension by running its executable, and
/// none of this can be seen without registering and launching it, which tests may not do in the user's session.
private let builtPlugIns = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("build/Seedwell.app/Contents/PlugIns")

@Suite(.timeLimit(.minutes(1)),
       .enabled(if: FileManager.default.fileExists(atPath: builtPlugIns.appendingPathComponent("SeedwellFileProvider.appex").path)))
struct ExtensionEntryTests {
    static let extensions = ["SeedwellFileProvider", "SeedwellFinderSync", "SeedwellShare"]

    /// Fails when an extension's executable has a Swift entry point (a `main.swift` or `@main`, which put a
    /// `__TEXT,__swift5_entry` section in it) or starts anywhere but in Foundation's `NSExtensionMain`. On an
    /// ExtensionKit extension point (`com.apple.fileprovider-nonui` since macOS 26), ExtensionFoundation, started
    /// from `NSExtensionMain`, calls the executable's Swift entry point: a Swift main that called `NSExtensionMain`
    /// was called again from inside it, until the stack ran out ("Thread stack size exceeded due to excessive
    /// recursion" at every launch, 0.1.13 to 0.1.19), so on-demand folders never started.
    @Test(arguments: extensions) func startsInNSExtensionMainWithNoSwiftEntryPoint(_ name: String) throws {
        let image = try MachOImage(Self.executable(name))
        #expect(image.section("__TEXT", "__swift5_entry") == nil,
                "\(name) has a Swift entry point, which ExtensionFoundation calls from inside NSExtensionMain")
        #expect(image.definesSymbol("_main") == false, "\(name) has a main of its own")
        #expect(try image.entrySymbol() == "_NSExtensionMain", "\(name) must be linked with -e _NSExtensionMain")
    }

    /// Fails when the class the extension's Info.plist names as its principal class is not in its executable (the
    /// linker dropped it, as nothing calls it by name), so `NSExtensionMain` has nothing to run.
    @Test(arguments: extensions) func thePrincipalClassIsInTheExecutable(_ name: String) throws {
        let bundle = try #require(Bundle(url: builtPlugIns.appendingPathComponent("\(name).appex")))
        let extensionInfo = try #require(bundle.object(forInfoDictionaryKey: "NSExtension") as? [String: Any])
        let principal = try #require(extensionInfo["NSExtensionPrincipalClass"] as? String)
        let image = try MachOImage(Self.executable(name))
        let classNames = image.section("__TEXT", "__objc_classname").map { image.strings(in: $0) } ?? []
        #expect(classNames.contains(principal), "\(principal) is not among \(name)'s Objective-C classes")
    }

    static func executable(_ name: String) -> URL {
        builtPlugIns.appendingPathComponent("\(name).appex/Contents/MacOS/\(name)")
    }
}

/// The few parts of a thin 64-bit Mach-O executable these tests read.
private struct MachOImage {
    struct Section { let segment: String, name: String, offset: Int, size: Int, kind: UInt32, firstIndirect: Int, stride: Int }
    let data: Data
    var sections: [Section] = []
    var entryOffset: Int?
    var symtab: symtab_command?
    var dysymtab: dysymtab_command?

    init(_ url: URL) throws {
        data = try Data(contentsOf: url)
        let header: mach_header_64 = data.read(at: 0)
        guard header.magic == 0xfeed_facf /* MH_MAGIC_64 */ else { throw Failure("\(url.lastPathComponent) is not a thin 64-bit Mach-O") }
        var at = MemoryLayout<mach_header_64>.size
        for _ in 0..<header.ncmds {
            let command: load_command = data.read(at: at)
            switch command.cmd {
            case 0x19 /* LC_SEGMENT_64 */:
                let segment: segment_command_64 = data.read(at: at)
                var sectionAt = at + MemoryLayout<segment_command_64>.size
                for _ in 0..<segment.nsects {
                    let s: section_64 = data.read(at: sectionAt)
                    sections.append(Section(segment: Self.name(s.segname), name: Self.name(s.sectname), offset: Int(s.offset),
                                            size: Int(s.size), kind: s.flags & 0xff /* SECTION_TYPE */,
                                            firstIndirect: Int(s.reserved1), stride: Int(s.reserved2)))
                    sectionAt += MemoryLayout<section_64>.size
                }
            case 0x8000_0028 /* LC_MAIN */: entryOffset = Int((data.read(at: at) as entry_point_command).entryoff)
            case 0x2 /* LC_SYMTAB */: symtab = data.read(at: at)
            case 0xb /* LC_DYSYMTAB */: dysymtab = data.read(at: at)
            default: break
            }
            at += Int(command.cmdsize)
        }
    }

    func section(_ segment: String, _ name: String) -> Section? {
        sections.first { $0.segment == segment && $0.name == name }
    }

    /// The NUL-separated strings of a section such as `__objc_classname`.
    func strings(in section: Section) -> [String] {
        data[section.offset..<section.offset + section.size].split(separator: 0).map { String(decoding: $0, as: UTF8.self) }
    }

    /// The symbol the process starts in: the one LC_MAIN points at, defined here or (as `-e _NSExtensionMain` makes
    /// it) a stub of a symbol imported from a library.
    func entrySymbol() throws -> String? {
        guard let entry = entryOffset else { throw Failure("no LC_MAIN") }
        guard let symtab, let dysymtab else { throw Failure("no symbol table") }
        if let stubs = sections.first(where: { $0.kind == 0x8 /* S_SYMBOL_STUBS */ && (($0.offset)..<($0.offset + $0.size)).contains(entry) }) {
            let slot = stubs.firstIndirect + (entry - stubs.offset) / max(stubs.stride, 1)
            let index: UInt32 = data.read(at: Int(dysymtab.indirectsymoff) + slot * 4)
            return symbolName(Int(index), symtab)
        }
        return nil  // the entry is in the executable's own code (a main of its own)
    }

    func definesSymbol(_ name: String) -> Bool {
        guard let symtab else { return false }
        return (0..<Int(symtab.nsyms)).contains { i in
            let symbol: nlist_64 = data.read(at: Int(symtab.symoff) + i * MemoryLayout<nlist_64>.size)
            return symbol.n_type & 0x0e /* N_TYPE */ == 0x0e /* N_SECT */ && symbolName(i, symtab) == name
        }
    }

    private func symbolName(_ index: Int, _ symtab: symtab_command) -> String {
        let symbol: nlist_64 = data.read(at: Int(symtab.symoff) + index * MemoryLayout<nlist_64>.size)
        let start = Int(symtab.stroff) + Int(symbol.n_un.n_strx)
        let end = data[start...].firstIndex(of: 0) ?? data.endIndex
        return String(decoding: data[start..<end], as: UTF8.self)
    }

    private static func name<T>(_ tuple: T) -> String {
        withUnsafeBytes(of: tuple) { String(decoding: $0.prefix { $0 != 0 }, as: UTF8.self) }
    }

    struct Failure: Error, CustomStringConvertible { let description: String; init(_ d: String) { description = d } }
}

private extension Data {
    func read<T>(at offset: Int) -> T {
        withUnsafeBytes { $0.loadUnaligned(fromByteOffset: offset, as: T.self) }
    }
}
