#!/usr/bin/env bash
# Quick Swift-only rebuild (after build.sh produced the Rust library and bindings): prints errors.
cd "$(dirname "$0")/.."
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk
swift build -c release --arch arm64 --product SeedwellApp 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -E "error:|warning: var|Compiling|Build complete" | grep -v "^\s*$" | sort -u | head -${1:-60}
