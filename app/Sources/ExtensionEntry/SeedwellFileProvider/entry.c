// The File Provider extension's executable (Package.swift). It has no main of its own: it is linked with
// `-e _NSExtensionMain`, as Xcode links every app extension, so the process starts in Foundation's NSExtensionMain,
// which finds the principal class (NSExtensionPrincipalClass, from the Swift module SeedwellFileProviderExtension)
// and runs it. Its code is in Swift, but this target is C on purpose:
//
// - On an ExtensionKit extension point (com.apple.fileprovider-nonui is one since macOS 26), ExtensionFoundation,
//   started from NSExtensionMain, calls the executable's Swift entry point (the __TEXT,__swift5_entry section that
//   a Swift main.swift or @main emits), meant for ExtensionKit's `@main struct …: AppExtension`. A Swift main that
//   called NSExtensionMain was called again and again, until the stack ran out (0.1.13 to 0.1.19).
// - SwiftPM's native build system links a Swift executable target with an alias of `_<module>_main` to `_main`,
//   which fails when the module has no entry point; it does not for a C target.
