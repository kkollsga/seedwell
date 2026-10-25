import Foundation

// An app extension starts in Foundation's NSExtensionMain, which sets up the extension and runs it (Xcode links
// extensions with `-e _NSExtensionMain`; SwiftPM builds a plain executable, so it is called from here).
typealias ExtensionMain = @convention(c) (Int32, UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "NSExtensionMain") else { fatalError("NSExtensionMain not found") }  // RTLD_DEFAULT
exit(unsafeBitCast(sym, to: ExtensionMain.self)(CommandLine.argc, CommandLine.unsafeArgv))
