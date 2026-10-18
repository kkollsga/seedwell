#!/usr/bin/env bash
# Rebuild the app (fast profile) and print only errors and the result line.
cd "$(dirname "$0")/.."
SEEDWELL_FAST=1 bash build.sh 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -E "error|Built |warning: unused|FAILED" | grep -v "was built for newer" | head -40
