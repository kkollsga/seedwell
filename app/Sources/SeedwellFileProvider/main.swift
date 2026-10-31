import Foundation

// An app extension starts in Foundation's NSExtensionMain (as app/Sources/SeedwellFinderSync/main.swift does).
typealias ExtensionMain = @convention(c) (Int32, UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "NSExtensionMain") else { fatalError("NSExtensionMain not found") }  // RTLD_DEFAULT
// The principal class must be in the binary under its Info.plist name: reference it so the linker keeps it.
_ = FileProviderExtension.self
exit(unsafeBitCast(sym, to: ExtensionMain.self)(CommandLine.argc, CommandLine.unsafeArgv))
