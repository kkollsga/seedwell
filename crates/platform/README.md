# trs-platform

The one place in Seedwell's core that names an operating system (docs/CROSS-PLATFORM.md §2). Every other crate asks
for what it means ("the volume of this path", "read at this offset", "stop when asked to") and gets the same answer on
macOS, Linux and Windows.

| Module | What |
|---|---|
| `fs` | Positioned reads and writes, volume ids, free space, renames that never replace, preallocation, no-cache, metadata copy, Unix modes (none on Windows), file stamps for Folder Sync (times, file id, exec bit, cloud placeholders), symlinks with `/` targets, folder sync, lock flags, path bytes for Seedwell's own files. |
| `dirs` | Home, Seedwell's state folder (Application Support, XDG state, `%LOCALAPPDATA%`), mount roots. |
| `proc` | Open-file limit, performance cores, background I/O priority, giving memory back, the local UTC offset, shell commands in a group of their own (process group, Job Object), detached commands. |
| `signal` | `Shutdown`: SIGTERM/SIGINT; Ctrl+C, console close, logoff, shutdown. |
| `net` | Sharing a UDP port between processes. |
| `ipc` | Named-pipe names for control addresses and owner-only pipes (Windows). |

Dependencies: `libc` (Unix), `windows-sys` (Windows), `socket2` (already used by sync-net), `tokio` (signals; named pipes on
Windows). This crate holds `unsafe` code; every block carries a `// SAFETY:` comment.
