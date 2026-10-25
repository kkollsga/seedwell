#!/usr/bin/env bash
# One command from a clean checkout: Rust core -> Swift bindings -> Swift app -> build/Seedwell.app
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$APP_DIR/.." && pwd)"
TARGET=aarch64-apple-darwin
# SEEDWELL_PROFILE=dev (CI): debug Rust and Swift for the host target, sharing the artifacts `cargo test` and
# `swift test` already built, so CI compiles everything once. Never for releases or measurements.
if [ "${SEEDWELL_PROFILE:-release}" = "dev" ]; then
    [ "$(uname -m)" = "arm64" ] || { echo "SEEDWELL_PROFILE=dev needs an Apple silicon host" >&2; exit 1; }
    CARGO_PROFILE_ARGS=(); RUST_OUT="$REPO/target/debug"; SWIFT_ARGS=(-c debug)   # no --arch: same build as `swift test`
else
    CARGO_PROFILE_ARGS=(--release --target "$TARGET"); RUST_OUT="$REPO/target/$TARGET/release"; SWIFT_ARGS=(-c release --arch arm64)
fi
BUILD="$APP_DIR/build"
APP="$BUILD/Seedwell.app"
# One version for everything: [workspace.package] version in Cargo.toml (also trs-cli --version, User-Agent,
# peer id). The build number is the git commit count: meaningful and monotonic.
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$REPO/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "no version in Cargo.toml [workspace.package]" >&2; exit 1; }
BUILD_NUMBER="${SEEDWELL_BUILD:-$(git -C "$REPO" rev-list --count HEAD 2>/dev/null || echo 1)}"
# Sparkle (SPEC 12 #19). The feed is a placeholder until the user publishes one (docs/RELEASING.md).
FEED_URL="${SEEDWELL_FEED_URL:-https://kkollsga.github.io/seedwell/appcast.xml}"
SPARKLE_KEY="${SEEDWELL_SPARKLE_PUBLIC_KEY:-}"
if [ -z "$SPARKLE_KEY" ] && [ -f "$APP_DIR/sparkle_public_key" ]; then SPARKLE_KEY="$(tr -d '[:space:]' < "$APP_DIR/sparkle_public_key")"; fi
# The GitHub-release updater (SPEC 12A): where releases live, and whether this is a release build
# (release-zip.sh exports SEEDWELL_RELEASE=1; only release builds check and install by themselves).
REPO_URL="${SEEDWELL_REPO_URL:-https://github.com/kkollsga/seedwell}"
if [ "${SEEDWELL_RELEASE:-0}" = "1" ]; then RELEASE_BUILD=true; else RELEASE_BUILD=false; fi
echo "==> Seedwell $VERSION (build $BUILD_NUMBER)"

# SEEDWELL_FAST=1: skip fat LTO for quick development builds (never for measurements or releases).
if [ "${SEEDWELL_FAST:-0}" = "1" ]; then
    export CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
    echo "    SEEDWELL_FAST: no LTO"
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}"

echo "==> Rust core (trs-ffi, ${SEEDWELL_PROFILE:-release})"
(cd "$REPO" && cargo build -p trs-ffi ${CARGO_PROFILE_ARGS[@]+"${CARGO_PROFILE_ARGS[@]}"})
LIB="$RUST_OUT/libtrs_ffi.a"
mkdir -p "$BUILD/rust"
cp "$LIB" "$BUILD/rust/libtrs_ffi.a"

echo "==> seedwell CLI (client + embedded skill, no engine)"
(cd "$REPO" && cargo build -p trs-cli ${CARGO_PROFILE_ARGS[@]+"${CARGO_PROFILE_ARGS[@]}"} --no-default-features)
CLI="$RUST_OUT/seedwell"

echo "==> Swift bindings (uniffi-bindgen)"
GEN="$BUILD/generated"
rm -rf "$GEN" && mkdir -p "$GEN"
(cd "$REPO" && cargo run -q -p trs-ffi --features bindgen --bin uniffi-bindgen -- \
    generate --library "$LIB" --language swift --out-dir "$GEN")
mkdir -p "$APP_DIR/Sources/trs_ffiFFI/include" "$APP_DIR/Sources/SeedwellApp/Generated"
cp "$GEN/trs_ffiFFI.h" "$APP_DIR/Sources/trs_ffiFFI/include/trs_ffiFFI.h"
cp "$GEN/trs_ffiFFI.modulemap" "$APP_DIR/Sources/trs_ffiFFI/include/module.modulemap"
cp "$GEN/trs_ffi.swift" "$APP_DIR/Sources/SeedwellApp/Generated/trs_ffi.swift"

# SwiftUI's @State is a macro in the newest SDKs and needs the plugin shipped with full Xcode.
# Without it (Command Line Tools only), fall back to the 26.x SDK, which needs no plugin.
if [ -z "${SDKROOT:-}" ]; then
    PROBE="$(mktemp -d)"
    printf 'import SwiftUI\nstruct V: View { @State var x = 1; var body: some View { Text("") } }\n' > "$PROBE/p.swift"
    if ! swiftc -typecheck "$PROBE/p.swift" >/dev/null 2>&1; then
        for sdk in /Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk /Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk; do
            if [ -d "$sdk" ]; then export SDKROOT="$sdk"; echo "    default SDK lacks the SwiftUI macro plugin; using $sdk"; break; fi
        done
    fi
    rm -rf "$PROBE"
fi

"$APP_DIR/fetch-sparkle.sh"
echo "==> Swift app (${SWIFT_ARGS[*]})"
(cd "$APP_DIR" && swift build "${SWIFT_ARGS[@]}" --product SeedwellApp)
(cd "$APP_DIR" && swift build "${SWIFT_ARGS[@]}" --product SeedwellFinderSync)
(cd "$APP_DIR" && swift build "${SWIFT_ARGS[@]}" --product SeedwellShare)
BIN_DIR="$(cd "$APP_DIR" && swift build "${SWIFT_ARGS[@]}" --show-bin-path)"
BIN="$BIN_DIR/SeedwellApp"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The app executable cannot be called `Seedwell`: APFS is case-insensitive and the CLI is `seedwell`.
cp "$BIN" "$APP/Contents/MacOS/SeedwellApp"
# The agent interface (SPEC 9A): the CLI, `seedwell skill install`; Settings > Agents offers to link it into the PATH.
cp "$CLI" "$APP/Contents/MacOS/seedwell"
echo "==> Sparkle.framework"
SPARKLE_FW="$(cd "$APP_DIR" && find .sparkle/Sparkle.xcframework -type d -name Sparkle.framework -path '*macos*' | head -1)"
[ -n "$SPARKLE_FW" ] || { echo "Sparkle.framework not found in app/.sparkle" >&2; exit 1; }
mkdir -p "$APP/Contents/Frameworks"
ditto "$APP_DIR/$SPARKLE_FW" "$APP/Contents/Frameworks/Sparkle.framework"
# Apple silicon only (the Rust core is built for aarch64): drop the x86_64 slices. sign.sh re-signs everything.
while IFS= read -r f; do
    if lipo -archs "$f" 2>/dev/null | grep -q x86_64; then lipo "$f" -thin arm64 -output "$f.thin" && mv "$f.thin" "$f"; fi
done < <(find "$APP/Contents/Frameworks/Sparkle.framework" -type f -perm +111 ! -name '*.plist')
echo "==> App icon"
swiftc -O "$APP_DIR/Resources/make_icon.swift" -o "$BUILD/make_icon" 2>&1 | tail -3
rm -rf "$BUILD/AppIcon.iconset" && "$BUILD/make_icon" "$BUILD/AppIcon.iconset"
iconutil -c icns "$BUILD/AppIcon.iconset" -o "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>io.github.kkollsga.Seedwell</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleName</key><string>Seedwell</string>
    <key>CFBundleDisplayName</key><string>Seedwell</string>
    <key>CFBundleExecutable</key><string>SeedwellApp</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$BUILD_NUMBER</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>LSUIElement</key><true/>
    <!-- Memory (SPEC 2): give freed pages back to the system instead of keeping them in per-CPU
         magazines. With the AppKit/SwiftUI state of a window that was opened and closed this is
         about 10 MB; loopback download throughput is unchanged (568 vs 559 MB/s). -->
    <key>LSEnvironment</key>
    <dict>
        <key>MallocSpaceEfficient</key><string>1</string>
        <key>MallocMaxMagazines</key><string>1</string>
    </dict>
    <!-- Sparkle. No automatic checks until the user switches them on, and the updater does not start at
         all without SUPublicEDKey: a default build makes no network request of its own. -->
    <!-- The GitHub-release updater, used when Sparkle is not configured. -->
    <key>SeedwellRepoURL</key><string>$REPO_URL</string>
    <key>SeedwellReleaseBuild</key><$RELEASE_BUILD/>
    <key>SUFeedURL</key><string>$FEED_URL</string>
    <key>SUPublicEDKey</key><string>$SPARKLE_KEY</string>
    <key>SUEnableAutomaticChecks</key><false/>
    <key>SUSendSystemProfile</key><false/>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>BitTorrent file</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>CFBundleTypeExtensions</key><array><string>torrent</string></array>
            <key>LSItemContentTypes</key><array><string>org.bittorrent.torrent</string></array>
            <key>LSHandlerRank</key><string>Default</string>
        </dict>
    </array>
    <key>UTImportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key><string>org.bittorrent.torrent</string>
            <key>UTTypeDescription</key><string>BitTorrent file</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string></array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key><array><string>torrent</string></array>
                <key>public.mime-type</key><array><string>application/x-bittorrent</string></array>
            </dict>
        </dict>
    </array>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key><string>Magnet link</string>
            <key>CFBundleURLSchemes</key><array><string>magnet</string></array>
        </dict>
        <dict>
            <!-- Folder sync links, seedwell://sync/<key>?name=… (SPEC 7G.1): they open the Add Folder modal. -->
            <key>CFBundleURLName</key><string>Seedwell folder link</string>
            <key>CFBundleURLSchemes</key><array><string>seedwell</string></array>
        </dict>
    </array>
    <!-- Folder sync finds devices on the LAN by UDP broadcast and multicast (SPEC 7G.3), which needs the Local
         Network permission (macOS 15+). Plain sockets, no Bonjour: NSBonjourServices is not needed, and macOS needs
         no multicast entitlement (TN3179). -->
    <key>NSLocalNetworkUsageDescription</key><string>Seedwell finds your other devices on the local network to sync folders with them.</string>
</dict>
</plist>
PLIST

echo "==> Finder extension"
# Badges on synced folders (SPEC 7G.5). It runs sandboxed and may read only the folder the app writes its status
# to: the app's own state, unless SEEDWELL_FINDER_STATUS_DIR (an absolute path) points a QA build elsewhere.
# sign.sh gives it the matching sandbox exception.
EXT="$APP/Contents/PlugIns/SeedwellFinderSync.appex"
mkdir -p "$EXT/Contents/MacOS"
cp "$BIN_DIR/SeedwellFinderSync" "$EXT/Contents/MacOS/SeedwellFinderSync"
FINDER_STATUS_DIR="${SEEDWELL_FINDER_STATUS_DIR:-~/Library/Application Support/Seedwell/Finder}"
cat > "$EXT/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>io.github.kkollsga.Seedwell.FinderSync</string>
    <key>CFBundleName</key><string>SeedwellFinderSync</string>
    <key>CFBundleDisplayName</key><string>Seedwell</string>
    <key>CFBundleExecutable</key><string>SeedwellFinderSync</string>
    <key>CFBundlePackageType</key><string>XPC!</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$BUILD_NUMBER</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHumanReadableDescription</key><string>Shows the sync status of Seedwell's folders in Finder.</string>
    <key>NSExtension</key>
    <dict>
        <key>NSExtensionAttributes</key><dict/>
        <key>NSExtensionPointIdentifier</key><string>com.apple.FinderSync</string>
        <key>NSExtensionPrincipalClass</key><string>SeedwellFinderSync</string>
    </dict>
    <!-- Read by the extension; sign.sh derives its sandbox exception from it. -->
    <key>SeedwellStatusDir</key><string>$FINDER_STATUS_DIR</string>
</dict>
</plist>
PLIST

echo "==> Share extension"
# Seedwell in the system Share menu (SPEC 7G.5): one file or folder; it hands the path to the app and needs no
# access of its own (sign.sh: sandbox only).
SHARE="$APP/Contents/PlugIns/SeedwellShare.appex"
mkdir -p "$SHARE/Contents/MacOS"
cp "$BIN_DIR/SeedwellShare" "$SHARE/Contents/MacOS/SeedwellShare"
cat > "$SHARE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>io.github.kkollsga.Seedwell.Share</string>
    <key>CFBundleName</key><string>SeedwellShare</string>
    <key>CFBundleDisplayName</key><string>Seedwell</string>
    <key>CFBundleExecutable</key><string>SeedwellShare</string>
    <key>CFBundlePackageType</key><string>XPC!</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$BUILD_NUMBER</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHumanReadableDescription</key><string>Shares a folder with Seedwell Folder Sync.</string>
    <key>NSExtension</key>
    <dict>
        <key>NSExtensionAttributes</key>
        <dict>
            <!-- One file or folder from the file system (not text, links or images from other apps). -->
            <key>NSExtensionActivationRule</key>
            <string>SUBQUERY(extensionItems, \$e, SUBQUERY(\$e.attachments, \$a, ANY \$a.registeredTypeIdentifiers UTI-CONFORMS-TO "public.file-url").@count == 1).@count == 1</string>
        </dict>
        <key>NSExtensionPointIdentifier</key><string>com.apple.share-services</string>
        <key>NSExtensionPrincipalClass</key><string>SeedwellShareViewController</string>
    </dict>
</dict>
</plist>
PLIST

# SwiftPM with Command Line Tools stamps LC_BUILD_VERSION sdk=15.0 (the deployment target), and
# macOS then runs the app in compatibility mode: no Liquid Glass sidebar or toolbar. Stamp the
# SDK we actually compiled against.
SDK_VER="$(xcrun --sdk "${SDKROOT:-macosx}" --show-sdk-version 2>/dev/null || true)"
if [ -n "$SDK_VER" ]; then
    vtool -set-build-version macos 15.0 "$SDK_VER" -replace -output "$APP/Contents/MacOS/SeedwellApp" "$APP/Contents/MacOS/SeedwellApp"
    vtool -set-build-version macos 15.0 "$SDK_VER" -replace -output "$EXT/Contents/MacOS/SeedwellFinderSync" "$EXT/Contents/MacOS/SeedwellFinderSync"
    vtool -set-build-version macos 15.0 "$SDK_VER" -replace -output "$SHARE/Contents/MacOS/SeedwellShare" "$SHARE/Contents/MacOS/SeedwellShare"
    echo "    LC_BUILD_VERSION: minos 15.0, sdk $SDK_VER"
fi

echo "==> Codesign"
"$APP_DIR/sign.sh" "$APP"
echo "Built $APP ($VERSION, build $BUILD_NUMBER)"
