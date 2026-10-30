//! Windows backend: `ReadDirectoryChangesW` on the root, recursive.
//!
//! One directory handle covers the whole tree (`bWatchSubtree`), so, unlike inotify, a new directory needs no
//! watch of its own; but the system reports only the directory, not what was in it when it was created or
//! moved in, so that is a [`RescanReason::NewDirectory`] rescan of it. Renames come as old-name/new-name
//! pairs and are reported as two [`Hint::Renamed`] halves directly. A directory's own "modified" record
//! (its entries changed) is left out: the entries' records say what changed, and a path event for a
//! directory makes the consumer walk it.
//!
//! Losses become rescans: a completion with no records (the system's buffer overflowed) or
//! `ERROR_NOTIFY_ENUM_DIR` is `Rescan { rel: None, reason: Dropped }`. The next read is posted into a second
//! buffer before the completed one is parsed, so a burst finds a read waiting. Reparse points (junctions,
//! symlinks) inside the root are not followed: the system reports changes to the link itself, never inside
//! its target, which is the scanner's rule too. A component reported in its 8.3 short form (`PROGRA~1`) that
//! does not exist under that name is a [`RescanReason::MustScanSubDirs`] of its parent rather than a guess.
//!
//! Once a second the thread checks that the root path still names the directory it watches (renamed,
//! deleted or replaced: [`RawEvent::RootChanged`]), and while it is missing looks for it again. No history:
//! a resume token means a full rescan. `unsafe` is only around the Win32 calls; records are parsed in safe
//! code from bytes.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects};

use crate::sink::Sink;
use crate::{Hint, RawEvent, RescanReason, ResumeToken, Started};

/// Bytes per read: the largest a network share accepts.
const BUF_BYTES: usize = 64 * 1024;

/// How often the thread looks at the root and sends an owed rescan (ms).
const TICK_MS: u32 = 1000;

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

/// Volume serial and file index: what a path names, comparable over time.
type FileId = (u32, u64);

fn file_id(h: HANDLE) -> Option<FileId> {
    // SAFETY: an all-zero struct is valid; `h` is an open handle for the call.
    unsafe {
        let mut i: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        (GetFileInformationByHandle(h, &mut i) != 0)
            .then(|| (i.dwVolumeSerialNumber, (u64::from(i.nFileIndexHigh) << 32) | u64::from(i.nFileIndexLow)))
    }
}

fn open(path: &Path, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle owned by nobody else.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// What the root path names now (`None`: nothing, or not a directory we can open).
fn root_id(root: &Path) -> Option<FileId> {
    let h = open(root, FILE_READ_ATTRIBUTES, 0).ok()?;
    std::fs::metadata(root).ok().filter(|m| m.is_dir())?;
    file_id(h.as_raw_handle())
}

fn event() -> io::Result<OwnedHandle> {
    // SAFETY: an unnamed manual-reset event, initially not set; owned below.
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle owned by nobody else.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// One read buffer and its OVERLAPPED. Boxed: both must stay where they are while a read is pending.
struct Slot {
    buf: Box<[u32]>,
    ov: Box<OVERLAPPED>,
    event: OwnedHandle,
    pending: bool,
}

// SAFETY: a Slot owns its buffer, OVERLAPPED and event outright (the OVERLAPPED's hEvent is the slot's own
// event); slots are made on the caller's thread, moved to the watcher thread whole and used only there.
unsafe impl Send for Slot {}

impl Slot {
    fn new() -> io::Result<Slot> {
        // SAFETY: an all-zero OVERLAPPED is valid.
        let ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        Ok(Slot { buf: vec![0u32; BUF_BYTES / 4].into_boxed_slice(), ov, event: event()?, pending: false })
    }
}

/// The watched root: its directory handle and what it is.
struct Dir {
    handle: OwnedHandle,
    id: FileId,
}

fn open_dir(root: &Path) -> io::Result<Dir> {
    let handle = open(root, FILE_LIST_DIRECTORY, FILE_FLAG_OVERLAPPED)?;
    let id = file_id(handle.as_raw_handle()).ok_or_else(io::Error::last_os_error)?;
    Ok(Dir { handle, id })
}

/// Posts a read into `s`. `false` when the system refused it (the directory is gone).
fn post(dir: &Dir, s: &mut Slot) -> bool {
    // SAFETY: a fresh OVERLAPPED with our event; the buffer and the OVERLAPPED are boxed and live until the
    // read completes or is cancelled and waited for (see `cancel`).
    unsafe {
        *s.ov = std::mem::zeroed();
        s.ov.hEvent = s.event.as_raw_handle();
        let ok = ReadDirectoryChangesW(
            dir.handle.as_raw_handle(),
            s.buf.as_mut_ptr().cast(),
            BUF_BYTES as u32,
            1,
            FILTER,
            std::ptr::null_mut(),
            &mut *s.ov,
            None,
        );
        s.pending = ok != 0;
    }
    s.pending
}

/// The result of a completed read: `Ok(bytes)`, or the error code.
fn result(dir: &Dir, s: &mut Slot) -> Result<usize, u32> {
    let mut n = 0u32;
    // SAFETY: the OVERLAPPED is the one this read was posted with; not waiting (the event said it is done).
    let ok = unsafe { GetOverlappedResult(dir.handle.as_raw_handle(), &*s.ov, &mut n, 0) };
    if ok != 0 {
        s.pending = false;
        return Ok(n as usize);
    }
    // SAFETY: reads this thread's last error, set by the call just before.
    let e = unsafe { GetLastError() };
    if e != ERROR_IO_INCOMPLETE {
        s.pending = false;
    }
    Err(e)
}

/// Cancels pending reads and waits for them, so the buffers are never written after this returns.
fn cancel(dir: &Dir, slots: &mut [Slot]) {
    for s in slots.iter_mut().filter(|s| s.pending) {
        let mut n = 0u32;
        // SAFETY: cancels and then waits for this slot's own read; the buffer and OVERLAPPED outlive both calls.
        unsafe {
            CancelIoEx(dir.handle.as_raw_handle(), &*s.ov);
            GetOverlappedResult(dir.handle.as_raw_handle(), &*s.ov, &mut n, 1);
        }
        s.pending = false;
    }
}

/// One `FILE_NOTIFY_INFORMATION` record: the action and the name relative to the watched directory.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub action: u32,
    pub name: OsString,
}

/// Splits a buffer of `FILE_NOTIFY_INFORMATION` records (NextEntryOffset, Action, FileNameLength in bytes,
/// FileName in UTF-16). A record that runs past the end ends the list (bounds checking only).
pub(crate) fn parse(buf: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let word = |i: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(i..i + 4)?.try_into().ok()?)) };
    while let (Some(next), Some(action), Some(len)) = (word(at), word(at + 4), word(at + 8)) {
        let Some(raw) = buf.get(at + 12..at + 12 + len as usize) else { break };
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        out.push(Record { action, name: OsString::from_wide(&units) });
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    out
}

/// The hint for a record's action.
pub(crate) fn hint(action: u32) -> Hint {
    match action {
        FILE_ACTION_ADDED => Hint::Created,
        FILE_ACTION_REMOVED => Hint::Removed,
        FILE_ACTION_MODIFIED => Hint::Modified,
        FILE_ACTION_RENAMED_OLD_NAME | FILE_ACTION_RENAMED_NEW_NAME => Hint::Renamed,
        _ => Hint::Unknown,
    }
}

/// Sends the events for one record. Returns `false` once the receiver is gone.
fn handle(root: &Path, r: &Record, sink: &mut Sink) -> bool {
    let rel = PathBuf::from(&r.name);
    let kind = hint(r.action);
    // A path that should be there now: what it is (links not followed).
    let there = matches!(r.action, FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME);
    let meta = there.then(|| std::fs::symlink_metadata(root.join(&rel)).ok()).flatten();
    if there && meta.is_none() && rel.components().any(|c| c.as_os_str().to_string_lossy().contains('~')) {
        // Perhaps an 8.3 short name; the parent's walk finds the real one.
        let parent = rel.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
        return sink.send(RawEvent::Rescan { rel: parent, reason: RescanReason::MustScanSubDirs });
    }
    let is_dir = meta.as_ref().map(|m| m.is_dir());
    if r.action == FILE_ACTION_MODIFIED && is_dir == Some(true) {
        return true; // its entries' own records say what changed
    }
    if !sink.send(RawEvent::Path { rel: rel.clone(), kind, is_dir }) {
        return false;
    }
    if is_dir == Some(true) && matches!(r.action, FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME) {
        return sink.send(RawEvent::Rescan { rel: Some(rel), reason: RescanReason::NewDirectory });
    }
    true
}

pub(crate) fn start(root: PathBuf, since: Option<ResumeToken>) -> io::Result<Started> {
    let (mut sink, events) = Sink::new();
    let mut dir = Some(open_dir(&root)?);
    let mut slots = [Slot::new()?, Slot::new()?];
    // The first read is posted before `watch` returns, so a change made right after it is seen.
    if !post(dir.as_ref().expect("just opened"), &mut slots[0]) {
        return Err(io::Error::last_os_error());
    }
    if since.is_some() {
        sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
        sink.send(RawEvent::HistoryDone);
    }
    let stop = event()?;
    // SAFETY (Send): the handle value is only used by the stop closure to set the event, which the thread owns
    // and outlives (the watcher joins the thread after setting it).
    let stop_raw = stop.as_raw_handle() as usize;
    let thread = std::thread::Builder::new().name("fswatch-rdcw".into()).spawn(move || {
        let mut cur = 0usize;
        // Every way out goes through the end of this loop, where pending reads are cancelled and waited for:
        // the buffers and OVERLAPPEDs are freed with the thread, and the system must not write into them after.
        'run: loop {
            let waiting = slots[cur].pending;
            let handles = [stop.as_raw_handle(), slots[cur].event.as_raw_handle()];
            let count = if waiting { 2 } else { 1 };
            // SAFETY: both handles are open events owned by this thread.
            let w = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, TICK_MS) };
            if w == WAIT_OBJECT_0 {
                break 'run;
            }
            if w == WAIT_TIMEOUT || !waiting {
                if !sink.flush() {
                    break 'run;
                }
                // The root: still the directory we watch? Back again?
                match (&dir, root_id(&root)) {
                    (Some(d), now) if now != Some(d.id) => {
                        let d = dir.take().expect("checked");
                        cancel(&d, &mut slots);
                        if !sink.send(RawEvent::RootChanged) {
                            break 'run;
                        }
                    }
                    (None, Some(_)) => {
                        if let Ok(d) = open_dir(&root) {
                            cur = 0;
                            if post(&d, &mut slots[0]) {
                                dir = Some(d);
                                if !sink.send(RawEvent::RootChanged) {
                                    break 'run;
                                }
                            }
                        }
                    }
                    (Some(d), _) if !slots.iter().any(|s| s.pending) && !crate::debug::reads_held() => {
                        // A read was refused earlier although the directory is still the same: try again, so the
                        // watcher never goes quiet for good (a rescan was sent when it was refused).
                        cur = 0;
                        let _ = post(d, &mut slots[0]);
                    }
                    _ => {}
                }
                continue;
            }
            // The read in `slots[cur]` completed.
            crate::debug::CALLBACKS.fetch_add(1, Ordering::SeqCst);
            let Some(d) = &dir else { continue };
            // SAFETY: the event is ours; a manual-reset event is cleared before the slot is used again.
            unsafe { ResetEvent(slots[cur].event.as_raw_handle()) };
            let done = result(d, &mut slots[cur]);
            if matches!(done, Err(ERROR_IO_INCOMPLETE)) {
                continue;
            }
            // Keep a read waiting while this one is parsed (unless a test holds reads back).
            let next = 1 - cur;
            if !crate::debug::reads_held() && !post(d, &mut slots[next]) {
                // The directory refused a new read: it is gone or no longer what it was; the tick says which.
                let _ = sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped });
            }
            match done {
                Ok(0) | Err(ERROR_NOTIFY_ENUM_DIR) => {
                    if !sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped }) {
                        break 'run;
                    }
                }
                Ok(n) => {
                    let bytes: Vec<u8> = slots[cur].buf.iter().flat_map(|w| w.to_le_bytes()).take(n).collect();
                    for r in parse(&bytes) {
                        if !handle(&root, &r, &mut sink) {
                            break 'run;
                        }
                    }
                }
                Err(ERROR_OPERATION_ABORTED) => {}
                Err(_) => {
                    if !sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped }) {
                        break 'run;
                    }
                }
            }
            if crate::debug::reads_held() {
                // Held back: no read is pending; post one as soon as the hold ends.
                while crate::debug::reads_held() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let _ = post(d, &mut slots[next]);
            }
            cur = next;
        }
        if let Some(d) = &dir {
            cancel(d, &mut slots);
        }
    })?;
    Ok(Started {
        events,
        stop: Box::new(move || {
            // SAFETY: the event outlives this call (the thread owns it and is joined after).
            unsafe { SetEvent(stop_raw as HANDLE) };
        }),
        thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_a_chain_of_records() {
        let mut a = record(0, FILE_ACTION_ADDED, "a.txt");
        a.extend_from_slice(&[0, 0, 0]); // padding to a DWORD boundary
        let off = a.len() as u32;
        a[..4].copy_from_slice(&off.to_le_bytes());
        a.extend(record(0, FILE_ACTION_RENAMED_NEW_NAME, "sub\\b.txt"));
        assert_eq!(
            parse(&a),
            vec![
                Record { action: FILE_ACTION_ADDED, name: "a.txt".into() },
                Record { action: FILE_ACTION_RENAMED_NEW_NAME, name: "sub\\b.txt".into() }
            ]
        );
        let cut = record(0, FILE_ACTION_REMOVED, "gone.txt");
        assert!(parse(&cut[..cut.len() - 3]).is_empty(), "a record cut short ends the list");
        assert!(parse(&[]).is_empty());
    }

    #[test]
    fn actions_map_to_hints() {
        assert_eq!(hint(FILE_ACTION_ADDED), Hint::Created);
        assert_eq!(hint(FILE_ACTION_REMOVED), Hint::Removed);
        assert_eq!(hint(FILE_ACTION_MODIFIED), Hint::Modified);
        assert_eq!(hint(FILE_ACTION_RENAMED_OLD_NAME), Hint::Renamed);
        assert_eq!(hint(FILE_ACTION_RENAMED_NEW_NAME), Hint::Renamed);
        assert_eq!(hint(99), Hint::Unknown);
    }
}
