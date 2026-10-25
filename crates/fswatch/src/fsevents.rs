//! macOS FSEvents backend, through our own bindings to CoreServices, CoreFoundation and libdispatch.
//!
//! Threads: FSEvents calls [`callback`] on a private serial dispatch queue. The watcher's std thread owns the
//! stream: it creates it, runs the resume-token timer and tears the stream down on stop. The two share
//! [`Shared`] (an `Arc`): the stream holds its own count through the context's retain/release callbacks, so
//! the pointer FSEvents hands back is valid for as long as the stream can call us.
//!
//! Stop: `stopped` is set, then the stream is stopped and invalidated (no new callbacks are scheduled), an
//! empty `dispatch_sync_f` on the queue waits for a callback that was already running or queued, and only
//! then are the stream and the queue released. So when [`crate::Watcher::stop`] returns, no callback runs.

use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::sink::Sink;
use crate::{Hint, RawEvent, RescanReason, ResumeToken, Started, WatchOptions, debug, relativize};

#[allow(non_upper_case_globals, non_camel_case_types, dead_code)]
mod ffi {
    use std::ffi::{c_char, c_void};

    pub type CFIndex = isize;
    pub type CFTypeRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type CFArrayRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFUUIDRef = *const c_void;
    pub type FSEventStreamRef = *mut c_void;
    pub type ConstFSEventStreamRef = *const c_void;
    pub type dispatch_queue_t = *mut c_void;

    pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
    pub const kFSEventStreamEventIdSinceNow: u64 = u64::MAX;

    // FSEventStreamCreateFlags
    pub const kFSEventStreamCreateFlagNoDefer: u32 = 0x02;
    pub const kFSEventStreamCreateFlagWatchRoot: u32 = 0x04;
    pub const kFSEventStreamCreateFlagFileEvents: u32 = 0x10;

    // FSEventStreamEventFlags
    pub const kFSEventStreamEventFlagMustScanSubDirs: u32 = 0x0000_0001;
    pub const kFSEventStreamEventFlagUserDropped: u32 = 0x0000_0002;
    pub const kFSEventStreamEventFlagKernelDropped: u32 = 0x0000_0004;
    pub const kFSEventStreamEventFlagEventIdsWrapped: u32 = 0x0000_0008;
    pub const kFSEventStreamEventFlagHistoryDone: u32 = 0x0000_0010;
    pub const kFSEventStreamEventFlagRootChanged: u32 = 0x0000_0020;
    pub const kFSEventStreamEventFlagMount: u32 = 0x0000_0040;
    pub const kFSEventStreamEventFlagUnmount: u32 = 0x0000_0080;
    pub const kFSEventStreamEventFlagItemCreated: u32 = 0x0000_0100;
    pub const kFSEventStreamEventFlagItemRemoved: u32 = 0x0000_0200;
    pub const kFSEventStreamEventFlagItemInodeMetaMod: u32 = 0x0000_0400;
    pub const kFSEventStreamEventFlagItemRenamed: u32 = 0x0000_0800;
    pub const kFSEventStreamEventFlagItemModified: u32 = 0x0000_1000;
    pub const kFSEventStreamEventFlagItemFinderInfoMod: u32 = 0x0000_2000;
    pub const kFSEventStreamEventFlagItemChangeOwner: u32 = 0x0000_4000;
    pub const kFSEventStreamEventFlagItemXattrMod: u32 = 0x0000_8000;
    pub const kFSEventStreamEventFlagItemIsFile: u32 = 0x0001_0000;
    pub const kFSEventStreamEventFlagItemIsDir: u32 = 0x0002_0000;
    pub const kFSEventStreamEventFlagItemIsSymlink: u32 = 0x0004_0000;
    pub const kFSEventStreamEventFlagOwnEvent: u32 = 0x0008_0000;
    pub const kFSEventStreamEventFlagItemIsHardlink: u32 = 0x0010_0000;
    pub const kFSEventStreamEventFlagItemIsLastHardlink: u32 = 0x0020_0000;
    pub const kFSEventStreamEventFlagItemCloned: u32 = 0x0040_0000;

    /// `void (*)(ConstFSEventStreamRef, void *info, size_t n, void *paths, const flags[], const ids[])`
    pub type FSEventStreamCallback = extern "C" fn(ConstFSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[repr(C)]
    pub struct FSEventStreamContext {
        pub version: CFIndex,
        pub info: *mut c_void,
        pub retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        pub release: Option<extern "C" fn(*const c_void)>,
        pub copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    /// `CFArrayCallBacks`: only its address is used.
    #[repr(C)]
    pub struct CFArrayCallBacks {
        pub version: CFIndex,
        pub retain: *const c_void,
        pub release: *const c_void,
        pub copy_description: *const c_void,
        pub equal: *const c_void,
    }

    /// `CFUUIDBytes`: sixteen `UInt8` fields, the same layout as `[u8; 16]`.
    #[repr(C)]
    pub struct CFUUIDBytes {
        pub bytes: [u8; 16],
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        pub fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *mut FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: u64,
            latency: f64,
            flags: u32,
        ) -> FSEventStreamRef;
        pub fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: dispatch_queue_t);
        pub fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
        pub fn FSEventStreamFlushSync(stream: FSEventStreamRef);
        pub fn FSEventStreamStop(stream: FSEventStreamRef);
        pub fn FSEventStreamInvalidate(stream: FSEventStreamRef);
        pub fn FSEventStreamRelease(stream: FSEventStreamRef);
        /// Takes no pointers and only reads a counter.
        pub safe fn FSEventsGetCurrentEventId() -> u64;
        pub fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFUUIDRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeArrayCallBacks: CFArrayCallBacks;
        pub fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFStringRef;
        pub fn CFArrayCreate(
            alloc: CFAllocatorRef,
            values: *const *const c_void,
            num_values: CFIndex,
            callbacks: *const CFArrayCallBacks,
        ) -> CFArrayRef;
        pub fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
        pub fn CFRelease(cf: CFTypeRef);
    }

    // libdispatch is part of libSystem, which every program links.
    unsafe extern "C" {
        pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> dispatch_queue_t;
        pub fn dispatch_sync_f(queue: dispatch_queue_t, context: *mut c_void, work: extern "C" fn(*mut c_void));
        pub fn dispatch_release(object: dispatch_queue_t);
    }
}

use ffi::*;

/// While the root is quiet, the token is advanced to the daemon's current event ID every this many resume
/// intervals, so the next start does not replay a long stretch of unrelated history.
const IDLE_ADVANCE_TICKS: u32 = 12;

/// State shared by the callback (dispatch queue) and the watcher thread.
struct Shared {
    root: PathBuf,
    stopped: AtomicBool,
    state: Mutex<State>,
}

struct State {
    sink: Sink,
    /// Replaying history (a token was given and `HistoryDone` has not come yet). No tokens meanwhile.
    replaying: bool,
    /// Highest event ID delivered to the sink.
    last_id: u64,
}

impl Shared {
    fn new(root: PathBuf, state: State) -> Arc<Shared> {
        debug::LIVE_STREAMS.fetch_add(1, Ordering::SeqCst);
        Arc::new(Shared { root, stopped: AtomicBool::new(false), state: Mutex::new(state) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Handles one callback's events, in order, under the lock (so a token the timer sends afterwards comes
    /// after them on the channel).
    fn handle(&self, events: Vec<(PathBuf, u32, u64)>) {
        let mut st = self.lock();
        for (abs, flags, id) in events {
            if flags & kFSEventStreamEventFlagHistoryDone != 0 {
                st.replaying = false;
            } else if flags & kFSEventStreamEventFlagEventIdsWrapped != 0 {
                st.last_id = id;
            } else {
                st.last_id = st.last_id.max(id);
            }
            if let Some(ev) = map_event(&self.root, &abs, flags) {
                st.sink.send(ev);
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        debug::LIVE_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One FSEvents event as a [`RawEvent`]. Pure.
fn map_event(root: &Path, abs: &Path, f: u32) -> Option<RawEvent> {
    let has = |bits: u32| f & bits != 0;
    if has(kFSEventStreamEventFlagUserDropped | kFSEventStreamEventFlagKernelDropped) {
        return Some(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped });
    }
    if has(kFSEventStreamEventFlagEventIdsWrapped) {
        return Some(RawEvent::Rescan { rel: None, reason: RescanReason::EventIdsWrapped });
    }
    if has(kFSEventStreamEventFlagHistoryDone) {
        return Some(RawEvent::HistoryDone);
    }
    if has(kFSEventStreamEventFlagRootChanged) {
        return Some(RawEvent::RootChanged);
    }
    let Some(rel) = relativize(root, abs) else {
        return Some(RawEvent::Rescan { rel: None, reason: RescanReason::UnknownPath });
    };
    const ACTIONS: u32 = kFSEventStreamEventFlagItemCreated
        | kFSEventStreamEventFlagItemRemoved
        | kFSEventStreamEventFlagItemInodeMetaMod
        | kFSEventStreamEventFlagItemRenamed
        | kFSEventStreamEventFlagItemModified
        | kFSEventStreamEventFlagItemFinderInfoMod
        | kFSEventStreamEventFlagItemChangeOwner
        | kFSEventStreamEventFlagItemXattrMod
        | kFSEventStreamEventFlagItemCloned;
    const TYPES: u32 = kFSEventStreamEventFlagItemIsFile | kFSEventStreamEventFlagItemIsDir | kFSEventStreamEventFlagItemIsSymlink;
    // A directory-level event (no item detail, as older history records are) or an explicit request:
    // the subtree must be looked at.
    if has(kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagMount | kFSEventStreamEventFlagUnmount)
        || f & (ACTIONS | TYPES) == 0
    {
        let rel = (!rel.as_os_str().is_empty()).then_some(rel);
        return Some(RawEvent::Rescan { rel, reason: RescanReason::MustScanSubDirs });
    }
    // Flags accumulate over everything that happened to the path recently, so pick the one that matters
    // most to the scanner; it stats the path anyway.
    let kind = if has(kFSEventStreamEventFlagItemRemoved) {
        Hint::Removed
    } else if has(kFSEventStreamEventFlagItemRenamed) {
        Hint::Renamed
    } else if has(kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemCloned) {
        Hint::Created
    } else if has(kFSEventStreamEventFlagItemModified) {
        Hint::Modified
    } else if has(
        kFSEventStreamEventFlagItemInodeMetaMod
            | kFSEventStreamEventFlagItemFinderInfoMod
            | kFSEventStreamEventFlagItemChangeOwner
            | kFSEventStreamEventFlagItemXattrMod,
    ) {
        Hint::Meta
    } else {
        Hint::Unknown
    };
    let is_dir = if has(kFSEventStreamEventFlagItemIsDir) {
        Some(true)
    } else if has(kFSEventStreamEventFlagItemIsFile | kFSEventStreamEventFlagItemIsSymlink) {
        Some(false)
    } else {
        None
    };
    Some(RawEvent::Path { rel, kind, is_dir })
}

extern "C" fn ctx_retain(info: *const c_void) -> *const c_void {
    // SAFETY: `info` is `Arc::as_ptr` of an `Arc<Shared>` (see `Stream::open`), and FSEvents only calls this
    // while it, or our thread, still holds a count, so the allocation is alive.
    unsafe { Arc::increment_strong_count(info.cast::<Shared>()) };
    info
}

extern "C" fn ctx_release(info: *const c_void) {
    // SAFETY: balances exactly one `ctx_retain` (FSEvents calls release once per retain).
    unsafe { Arc::decrement_strong_count(info.cast::<Shared>()) };
}

extern "C" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    debug::CALLBACKS.fetch_add(1, Ordering::SeqCst);
    if info.is_null() || n == 0 || paths.is_null() || flags.is_null() || ids.is_null() {
        return;
    }
    // SAFETY: `info` is the context pointer, which the stream keeps alive through `ctx_retain` until it is
    // released, and the stream only calls us before that. `Shared` is only used through shared references.
    let shared = unsafe { &*info.cast::<Shared>() };
    if shared.stopped.load(Ordering::SeqCst) {
        return;
    }
    // SAFETY: FSEvents passes `n` entries in each array, valid for the duration of the call. Without
    // `kFSEventStreamCreateFlagUseCFTypes`, `paths` is a `char **` of NUL-terminated C strings.
    let (paths, flags, ids) = unsafe {
        (
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), n),
            std::slice::from_raw_parts(flags, n),
            std::slice::from_raw_parts(ids, n),
        )
    };
    let mut events = Vec::with_capacity(n);
    for ((&p, &f), &id) in paths.iter().zip(flags).zip(ids) {
        let abs = if p.is_null() {
            PathBuf::new()
        } else {
            // SAFETY: a non-null entry of `paths` is a NUL-terminated string owned by FSEvents for this call;
            // the bytes are copied before returning.
            let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
            PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
        };
        events.push((abs, f, id));
    }
    // An unwind must not cross into FSEvents (it would abort). `handle` does not panic by design.
    let _ = catch_unwind(AssertUnwindSafe(|| shared.handle(events)));
}

extern "C" fn noop(_: *mut c_void) {}

/// An open stream on its own serial queue. Dropping it stops it; see the module docs for the order.
struct Stream {
    stream: FSEventStreamRef,
    queue: dispatch_queue_t,
    started: bool,
}

impl Stream {
    fn open(shared: &Arc<Shared>, since: u64, latency: Duration) -> io::Result<Stream> {
        let bytes = shared.root.as_os_str().as_bytes();
        let len = CFIndex::try_from(bytes.len()).map_err(|_| io::Error::other("root path too long"))?;
        // SAFETY: `bytes` is valid for `len` bytes for the call; CoreFoundation copies them.
        let path = unsafe { CFStringCreateWithBytes(std::ptr::null(), bytes.as_ptr(), len, kCFStringEncodingUTF8, 0) };
        if path.is_null() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the root path is not valid UTF-8"));
        }
        let values = [path];
        // SAFETY: `values` holds one valid CFString; kCFTypeArrayCallBacks makes the array retain it. Taking the
        // address of the extern static does not read it.
        let paths = unsafe { CFArrayCreate(std::ptr::null(), values.as_ptr(), 1, &raw const kCFTypeArrayCallBacks) };
        // SAFETY: we own the reference from CFStringCreateWithBytes; the array (if created) holds its own.
        unsafe { CFRelease(path) };
        if paths.is_null() {
            return Err(io::Error::other("CFArrayCreate failed"));
        }
        let mut ctx = FSEventStreamContext {
            version: 0,
            info: Arc::as_ptr(shared).cast_mut().cast::<c_void>(),
            retain: Some(ctx_retain),
            release: Some(ctx_release),
            copy_description: None,
        };
        let flags = kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagWatchRoot | kFSEventStreamCreateFlagNoDefer;
        // SAFETY: `ctx` is valid for the call (FSEvents copies it and retains `info` through `ctx_retain`, while
        // our `shared` keeps the Arc alive meanwhile); `paths` is a CFArray of CFStrings; `callback` has the
        // documented signature.
        let stream = unsafe {
            FSEventStreamCreate(std::ptr::null(), callback, &mut ctx, paths, since, latency.as_secs_f64(), flags)
        };
        // SAFETY: we own the array's reference; the stream copied what it needs.
        unsafe { CFRelease(paths) };
        if stream.is_null() {
            return Err(io::Error::other("FSEventStreamCreate failed"));
        }
        // SAFETY: the label is a NUL-terminated static string; a null attribute asks for a serial queue.
        let queue = unsafe { dispatch_queue_create(c"io.github.kkollsga.Seedwell.fswatch".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            // SAFETY: a valid, never scheduled stream that we own.
            unsafe { FSEventStreamRelease(stream) };
            return Err(io::Error::other("dispatch_queue_create failed"));
        }
        let mut s = Stream { stream, queue, started: false };
        // SAFETY: a valid stream and queue, both owned by `s`; the queue outlives the stream (released after it
        // in `drop`).
        unsafe { FSEventStreamSetDispatchQueue(s.stream, s.queue) };
        // SAFETY: a valid stream scheduled on a queue.
        if unsafe { FSEventStreamStart(s.stream) } == 0 {
            return Err(io::Error::other("FSEventStreamStart failed"));
        }
        s.started = true;
        Ok(s)
    }

    /// Delivers every event the daemon holds for this stream and returns once their callbacks are done. Must
    /// not be called with the [`Shared`] lock held (the callback takes it).
    fn flush_sync(&self) {
        // SAFETY: a valid, started stream; we are not on its queue (that would deadlock).
        unsafe { FSEventStreamFlushSync(self.stream) };
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: `stream` and `queue` are valid and owned by us. Stop (if started) and invalidate end all
        // scheduling; the empty synchronous block on the serial queue waits for a callback already running or
        // queued; after that nothing refers to the stream and it can be released, then the queue.
        unsafe {
            if self.started {
                FSEventStreamStop(self.stream);
            }
            FSEventStreamInvalidate(self.stream);
            dispatch_sync_f(self.queue, std::ptr::null_mut(), noop);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
    }
}

/// The UUID of the volume's FSEvents store, or `None` when it has none (history disabled, read-only or
/// unsupported volume): then event IDs from there cannot be resumed.
fn volume_uuid(dev: u64) -> Option<[u8; 16]> {
    // `st_dev` is an `i32` on macOS; `MetadataExt::dev` widened it, so the cast restores the original value.
    let dev = dev as libc::dev_t;
    // SAFETY: a plain call; it returns a CFUUID we own (create rule) or null.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(dev) };
    if uuid.is_null() {
        return None;
    }
    // SAFETY: `uuid` is a valid CFUUID; we read it and then release our reference.
    let bytes = unsafe {
        let b = CFUUIDGetUUIDBytes(uuid);
        CFRelease(uuid);
        b.bytes
    };
    (bytes != [0; 16]).then_some(bytes)
}

pub(crate) fn start(root: PathBuf, opts: &WatchOptions) -> io::Result<Started> {
    let (sink, events) = Sink::new();
    let (ready_tx, ready_rx) = sync_channel::<io::Result<()>>(1);
    let (stop_tx, stop_rx) = channel::<()>();
    let opts = opts.clone();
    let thread = std::thread::Builder::new()
        .name("fswatch-fsevents".into())
        .spawn(move || run(root, opts, sink, ready_tx, stop_rx))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Started { events, stop: Box::new(move || drop(stop_tx)), thread }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(io::Error::other("the FSEvents thread ended during setup"))
        }
    }
}

fn run(root: PathBuf, opts: WatchOptions, mut sink: Sink, ready: SyncSender<io::Result<()>>, stop: Receiver<()>) {
    let dev = match std::fs::metadata(&root) {
        Ok(m) => m.dev(),
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let uuid = volume_uuid(dev);
    let now_id = FSEventsGetCurrentEventId();
    let mut since = kFSEventStreamEventIdSinceNow;
    let mut last_id = now_id;
    if let Some(t) = opts.since {
        // An ID from the future means the store was reset under the same UUID: no usable history either.
        if uuid == Some(t.volume_uuid) && t.event_id <= now_id {
            since = t.event_id;
            last_id = t.event_id;
        } else {
            sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
            sink.send(RawEvent::HistoryDone);
        }
    }
    let replaying = since != kFSEventStreamEventIdSinceNow;
    let shared = Shared::new(root, State { sink, replaying, last_id });
    let stream = match Stream::open(&shared, since, opts.latency) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let tick = opts.resume_interval.max(Duration::from_millis(10));
    let mut sent: Option<u64> = None;
    let mut ticks: u32 = 0;
    // A message or the sender being dropped both mean stop.
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(tick) {
        ticks = ticks.wrapping_add(1);
        let replaying = {
            let mut st = shared.lock();
            st.sink.flush();
            st.replaying
        };
        let Some(volume_uuid) = uuid else { continue };
        if replaying {
            continue;
        }
        // Advancing through a quiet spell: once FlushSync returns, every event the daemon had numbered up to
        // `cur` that concerns this root has been handed to the sink.
        let mut floor = 0;
        if ticks.is_multiple_of(IDLE_ADVANCE_TICKS) {
            let cur = FSEventsGetCurrentEventId();
            stream.flush_sync();
            floor = cur;
        }
        let mut st = shared.lock();
        let id = st.last_id.max(floor);
        if sent != Some(id) {
            st.sink.send(RawEvent::Resume(ResumeToken { event_id: id, volume_uuid }));
            sent = Some(id);
            st.last_id = id;
        }
    }
    shared.stopped.store(true, Ordering::SeqCst);
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/private/var/folders/x/Sync";

    fn ev(abs: &str, f: u32) -> Option<RawEvent> {
        map_event(Path::new(ROOT), Path::new(abs), f)
    }

    #[test]
    fn item_flags_map_to_hints() {
        let file = kFSEventStreamEventFlagItemIsFile;
        let p = |rel: &str, kind, is_dir| Some(RawEvent::Path { rel: PathBuf::from(rel), kind, is_dir });
        assert_eq!(ev("/private/var/folders/x/Sync/a", kFSEventStreamEventFlagItemCreated | file), p("a", Hint::Created, Some(false)));
        assert_eq!(
            ev("/private/var/folders/x/Sync/d/", kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemIsDir),
            p("d", Hint::Created, Some(true))
        );
        assert_eq!(
            ev(
                "/private/var/folders/x/Sync/a",
                kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemModified | kFSEventStreamEventFlagItemRemoved | file
            ),
            p("a", Hint::Removed, Some(false))
        );
        assert_eq!(ev("/private/var/folders/x/Sync/a", kFSEventStreamEventFlagItemRenamed | file), p("a", Hint::Renamed, Some(false)));
        assert_eq!(ev("/private/var/folders/x/Sync/a", kFSEventStreamEventFlagItemXattrMod | file), p("a", Hint::Meta, Some(false)));
        assert_eq!(ev("/private/var/folders/x/Sync/a", file), p("a", Hint::Unknown, Some(false)));
    }

    #[test]
    fn special_flags_map_to_rescans() {
        let r = |rel: Option<&str>, reason| Some(RawEvent::Rescan { rel: rel.map(PathBuf::from), reason });
        assert_eq!(ev("/private/var/folders/x/Sync/a/", kFSEventStreamEventFlagMustScanSubDirs), r(Some("a"), RescanReason::MustScanSubDirs));
        assert_eq!(ev("/private/var/folders/x/Sync/", kFSEventStreamEventFlagMustScanSubDirs), r(None, RescanReason::MustScanSubDirs));
        assert_eq!(ev("/private/var/folders/x/Sync/a/", 0), r(Some("a"), RescanReason::MustScanSubDirs));
        assert_eq!(
            ev("/private/var/folders/x/Sync/a/", kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped),
            r(None, RescanReason::Dropped)
        );
        assert_eq!(ev("", kFSEventStreamEventFlagKernelDropped), r(None, RescanReason::Dropped));
        assert_eq!(ev("", kFSEventStreamEventFlagEventIdsWrapped), r(None, RescanReason::EventIdsWrapped));
        assert_eq!(ev("/elsewhere/a", kFSEventStreamEventFlagItemCreated), r(None, RescanReason::UnknownPath));
        assert_eq!(ev("", kFSEventStreamEventFlagHistoryDone), Some(RawEvent::HistoryDone));
        assert_eq!(ev(ROOT, kFSEventStreamEventFlagRootChanged), Some(RawEvent::RootChanged));
        assert_eq!(ev("/private/var/folders/x/Sync/vol", kFSEventStreamEventFlagMount), r(Some("vol"), RescanReason::MustScanSubDirs));
    }
}
