import Foundation

// An app extension starts in Foundation's NSExtensionMain (see SeedwellFinderSync/main.swift).
typealias ExtensionMain = @convention(c) (Int32, UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "NSExtensionMain") else { fatalError("NSExtensionMain not found") }  // RTLD_DEFAULT
exit(unsafeBitCast(sym, to: ExtensionMain.self)(CommandLine.argc, CommandLine.unsafeArgv))
