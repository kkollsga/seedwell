#!/usr/bin/env bash
# Tears a QA scene down (QA_ROOT, default the standard scene's): the app, the headless seeders and leechers, the
# loopback trackers, every other process whose command line names the root, and its throwaway disk images. The data
# under the QA root stays (delete the folder when it is not needed); nothing outside it was ever touched.
source "$(dirname "$0")/env.sh"
qa_guard
qa_teardown
echo "QA scene torn down ($QA_ROOT kept)"
