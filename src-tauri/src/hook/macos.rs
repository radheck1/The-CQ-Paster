//! The macOS global keyboard hook: a `CGEventTap` plus the chord state machine.
//!
//! Mirrors `hook.rs::windows_impl` in shape — a minimal callback that classifies
//! a keystroke and sends an [`Action`] down a channel, and a separate worker
//! thread that does every slow thing. That split is the most important
//! constraint in the codebase and it matters *more* here than on Windows: a slow
//! Windows hook is bypassed for one keystroke, whereas a slow event tap is
//! **disabled outright** by the system until something re-arms it.
//!
//! The two implementations are deliberately not factored into shared code.
//! Windows is shipping and cannot be re-verified from a macOS machine, so the
//! duplication buys the guarantee that nothing here can affect that build.
//!
//! # Why raw FFI rather than the `core-graphics` wrapper
//!
//! Suppressing a key requires the tap callback to return `NULL`. The safe
//! `CGEventTap` wrapper returns the *original* event when its closure yields
//! `None`, so it can rewrite events but can never swallow one — which is
//! precisely what `Cmd+<N>` needs. The FFI below is the minimum needed to
//! express that.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::clipboard;
use crate::{AppState, Mode};

/// Longest we will wait for the foreground app to publish its copy.
///
/// The Windows hook sleeps a flat 120ms here and hopes. We instead wait for the
/// copy to actually arrive — see `capture_after_copy` — so this is only the
/// point at which we give up, not an expected cost. Fast apps are served in a
/// poll interval or two.
const COPY_WAIT_MAX: Duration = Duration::from_millis(500);
/// How often to check the change counter while waiting.
const COPY_POLL: Duration = Duration::from_millis(10);
/// How long to leave the "injecting" guard up so our synthetic keystrokes pass
/// through the tap untouched.
const INJECT_GUARD_MS: u64 = 40;
/// How long to let the target read the pasteboard before we put the user's own
/// clipboard back. macOS offers no "the paste completed" signal, so unlike the
/// copy path this cannot wait on an outcome. Too short and the target pastes
/// the handed-back content instead of the slot; too long and a quick plain
/// Cmd+V lands before the handback.
const PASTE_HANDBACK_DELAY: Duration = Duration::from_millis(180);
/// How often the popup re-reads the cursor while it is up. 30Hz reads as
/// attached to the pointer while halving the main-thread traffic, since each
/// reposition has to hop to the main thread. The `CGEventCreate` per tick
/// happens on a worker thread, never in the tap callback.
const POPUP_FOLLOW: Duration = Duration::from_millis(33);
/// Safety net for a popup that is never told to hide. The Cmd release can be
/// missed while a synthetic paste is in flight (the same hazard 6.2 describes),
/// and a popup stuck on screen forever is far worse than one that vanishes late.
const POPUP_MAX_VISIBLE: Duration = Duration::from_secs(10);
/// How often to re-check for Accessibility permission before the tap exists.
const PERMISSION_POLL: Duration = Duration::from_secs(2);

// ---- Foundation / CoreGraphics FFI -------------------------------------------

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CGEventTapProxy = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

// Event types (`CGEventTypes.h`).
const KCG_EVENT_KEY_DOWN: u32 = 10;
const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const KCG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// Tap placement / options.
const KCG_SESSION_EVENT_TAP: u32 = 1;
const KCG_HID_EVENT_TAP: u32 = 0;
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KCG_EVENT_SOURCE_HID_SYSTEM_STATE: i32 = 1;

// Modifier flag masks and the keycode field id.
const KCG_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const KCG_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
const KCG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
/// Non-zero when a key event is the OS repeating a held key.
const KCG_KEYBOARD_EVENT_AUTOREPEAT: u32 = 8;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(port: CFMachPortRef, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateKeyboardEvent(src: CGEventSourceRef, keycode: u16, down: bool) -> CGEventRef;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFMachPortCreateRunLoopSource(
        alloc: CFAllocatorRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

/// The live tap's mach port, so the callback can re-arm it after macOS disables
/// it. A raw pointer in a static because the callback is registered as a plain
/// `extern "C"` function and cannot borrow the tap it belongs to.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// How many times a fresh Cmd press has cleared a pending digit. Diagnostic
/// only: a single atomic increment is cheap enough for the callback, whereas
/// logging there would get the tap disabled outright (6.1). Read and reported
/// by the worker under CQ_DEBUG.
static CHORD_RESETS: AtomicUsize = AtomicUsize::new(0);

/// Trace chord activity to stderr. Off unless `CQ_DEBUG=1`, and only ever
/// called from the worker thread.
fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CQ_DEBUG").is_ok_and(|v| v == "1"))
}

// ---- Chord machine -----------------------------------------------------------

enum Action {
    /// Cmd+digit pressed: show the reference popup (Noob mode).
    Peek(usize, f64, f64),
    /// Cmd+<N>+C: copy the current selection into slot N.
    Copy(usize),
    /// Cmd+<N>+V: paste slot N. `plain` (Shift held) strips formatting.
    Paste(usize, bool),
    /// Cmd released: the chord is over, so the popup comes down.
    ChordEnd,
}

/// Everything the callback needs, reached through the tap's `user_info`
/// pointer. Leaked deliberately: it must outlive the tap, which runs for the
/// life of the process.
struct Ctx {
    tx: Sender<Action>,
    injecting: Arc<AtomicBool>,
    /// The only cached chord state: which digit was pressed; 0 = none.
    ///
    /// The Cmd state is deliberately NOT cached — it is read from each event's
    /// own flags. A modifier release can be missed while a synthetic paste is
    /// in flight, and a stale flag would then misread a later plain `Cmd+C` as
    /// a slot store, silently clobbering a saved slot.
    pending_slot: AtomicUsize,
}

pub fn start(app: AppHandle, state: Arc<AppState>) {
    let (tx, rx) = mpsc::channel::<Action>();
    let injecting = Arc::new(AtomicBool::new(false));

    {
        let injecting = injecting.clone();
        thread::spawn(move || worker(app, state, rx, injecting));
    }
    {
        let injecting = injecting.clone();
        thread::spawn(move || run_tap(tx, injecting));
    }
}

/// Is the process allowed to observe and synthesise input?
///
/// In a dev build this is granted to the **terminal or IDE that launched the
/// binary**, not to CQ Paster — so a rebuild can appear to lose the permission,
/// and the entry to tick in System Settings is the terminal's.
/// Show the system's Accessibility prompt and report whether we are trusted.
pub fn request_accessibility() -> bool {
    accessibility_trusted(true)
}

fn accessibility_trusted(prompt: bool) -> bool {
    unsafe {
        if !prompt {
            return AXIsProcessTrustedWithOptions(std::ptr::null());
        }
        let keys = [kAXTrustedCheckOptionPrompt as CFTypeRef];
        let values = [kCFBooleanTrue];
        let opts = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
        );
        let trusted = AXIsProcessTrustedWithOptions(opts);
        if !opts.is_null() {
            CFRelease(opts);
        }
        trusted
    }
}

fn run_tap(tx: Sender<Action>, injecting: Arc<AtomicBool>) {
    // Wait quietly for the permission; `permissions.rs` owns all the prompting,
    // so this never raises its own dialog — two flows asking for the same grant
    // would stack a system prompt behind our alert. Polling means a permission
    // granted minutes later starts working with no restart.
    if !accessibility_trusted(false) {
        eprintln!(
            "[cq-paster] waiting for Accessibility permission (System Settings > \
             Privacy & Security > Accessibility). Hotkeys are inactive until then. \
             In a dev build, grant it to the terminal running this binary, not to \
             CQ Paster."
        );
        while !accessibility_trusted(false) {
            thread::sleep(PERMISSION_POLL);
        }
        eprintln!("[cq-paster] Accessibility granted; installing event tap");
    }

    // The context outlives the tap for the life of the process.
    let ctx = Box::into_raw(Box::new(Ctx {
        tx,
        injecting,
        pending_slot: AtomicUsize::new(0),
    }));

    loop {
        install_tap(ctx);
        // Only reached if the tap could not be created or its run loop exited;
        // the commonest cause is the permission being revoked.
        thread::sleep(PERMISSION_POLL);
    }
}

fn install_tap(ctx: *mut Ctx) {
    let mask = (1u64 << KCG_EVENT_KEY_DOWN) | (1u64 << KCG_EVENT_FLAGS_CHANGED);

    let port = unsafe {
        CGEventTapCreate(
            // Session level suppresses before apps see the key without needing
            // the extra entitlement kCGHIDEventTap wants.
            KCG_SESSION_EVENT_TAP,
            KCG_HEAD_INSERT_EVENT_TAP,
            // Default (not ListenOnly) is what allows swallowing an event.
            KCG_EVENT_TAP_OPTION_DEFAULT,
            mask,
            tap_callback,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        eprintln!("[cq-paster] could not create event tap (Accessibility revoked?)");
        return;
    }

    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0) };
    if source.is_null() {
        eprintln!("[cq-paster] could not create a run loop source for the tap");
        unsafe { CFRelease(port as CFTypeRef) };
        return;
    }

    TAP_PORT.store(port, Ordering::SeqCst);
    unsafe {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
        CFRunLoopRun(); // blocks until the tap dies
        CFRelease(source as CFTypeRef);
        CFRelease(port as CFTypeRef);
    }
    TAP_PORT.store(std::ptr::null_mut(), Ordering::SeqCst);
}

/// IMPORTANT: keep this callback minimal and non-blocking.
///
/// macOS times every tap callback and, if one runs long, silently disables the
/// tap — every hotkey stops working until something re-arms it. A single
/// `println!` here is enough to cause that. Classify, `send()`, return.
/// All real work belongs in [`worker`].
///
/// Returning a null pointer swallows the event; returning `event` passes it on.
extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    // The system disabled us — re-arm. This is the analogue of the Windows
    // LowLevelHooksTimeout bypass, except macOS does not recover on its own.
    if event_type == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        let port = TAP_PORT.load(Ordering::SeqCst);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
        }
        return event;
    }

    if user_info.is_null() {
        return event;
    }
    let ctx = unsafe { &*(user_info as *const Ctx) };

    // Let our own synthetic keystrokes through without reprocessing.
    if ctx.injecting.load(Ordering::SeqCst) {
        return event;
    }

    let flags = unsafe { CGEventGetFlags(event) };
    let keycode = unsafe { CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) };

    if event_type == KCG_EVENT_FLAGS_CHANGED {
        if is_command_key(keycode) {
            if (flags & KCG_FLAG_MASK_COMMAND) != 0 {
                // A fresh Cmd press begins a new chord: forget any leftover
                // digit so a plain Cmd+C / Cmd+V can never reuse a stale slot.
                ctx.pending_slot.store(0, Ordering::SeqCst);
                CHORD_RESETS.fetch_add(1, Ordering::Relaxed);
            } else {
                // Cmd released — the chord is over and the popup comes down.
                // Sent unconditionally rather than only after a digit: cheap,
                // and it means a missed arm can't strand the popup on screen.
                let _ = ctx.tx.send(Action::ChordEnd);
            }
        }
        return event;
    }

    // Only act while Cmd is held. Read from this event's own flags, never from
    // a cached modifier state (see `Ctx::pending_slot`).
    if (flags & KCG_FLAG_MASK_COMMAND) == 0 {
        return event;
    }

    if let Some(d) = digit_of(keycode) {
        // Arm on the initial press only. Holding Cmd+<N> repeats the digit at
        // the OS repeat rate, and a repeat arriving *after* the C or V consumed
        // the slot would silently re-arm it — turning the user's next plain
        // Cmd+V into another chord paste. Repeats are still swallowed, so the
        // app keeps owning Cmd+<N>.
        let repeat =
            unsafe { CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_AUTOREPEAT) } != 0;
        if !repeat {
            ctx.pending_slot.store(d, Ordering::SeqCst);
            let at = unsafe { CGEventGetLocation(event) };
            let _ = ctx.tx.send(Action::Peek(d, at.x, at.y));
        }
        return std::ptr::null_mut(); // swallow the digit
    }

    match keycode {
        VK_C => {
            // `swap` consumes the digit: a chord arms exactly one action. If
            // the slot merely stayed armed, the next plain Cmd+C would be read
            // as a slot store and would silently overwrite what was saved.
            // Correctness must not depend on observing a modifier release,
            // which is exactly the state that goes missing during injection.
            let slot = ctx.pending_slot.swap(0, Ordering::SeqCst);
            if slot != 0 {
                let _ = ctx.tx.send(Action::Copy(slot));
            }
            event // let Cmd+C perform the real copy
        }
        VK_V => {
            let slot = ctx.pending_slot.swap(0, Ordering::SeqCst);
            if slot != 0 {
                let plain = (flags & KCG_FLAG_MASK_SHIFT) != 0;
                let _ = ctx.tx.send(Action::Paste(slot, plain));
                return std::ptr::null_mut(); // swallow V; the worker pastes
            }
            event
        }
        _ => event,
    }
}

/// Capture the pasteboard once it actually holds the copy.
///
/// Waiting on `changeCount` alone is not enough. The counter bumps when the
/// source app calls `clearContents`/`declareTypes`, which happens *before* it
/// writes the payloads — so a capture taken on the bump alone finds the types
/// declared and empty. That produced a slot previewing as "(empty text)", and
/// then a restore with nothing to write, and so no paste at all.
///
/// So the wait is on the outcome, not on a duration: poll until a snapshot
/// actually carries bytes. Returns `None` if nothing usable ever appeared, in
/// which case the caller must leave the slot alone rather than overwrite a good
/// slot with an empty capture.
fn capture_after_copy() -> Option<crate::clipboard::ClipSnapshot> {
    let before = clipboard::change_count();
    let deadline = std::time::Instant::now() + COPY_WAIT_MAX;
    let mut changed = false;
    let mut best: Option<crate::clipboard::ClipSnapshot> = None;

    while std::time::Instant::now() < deadline {
        thread::sleep(COPY_POLL);
        if !changed && clipboard::change_count() != before {
            changed = true;
        }
        if !changed {
            continue;
        }
        // The counter moved; the payloads may still be on their way.
        if let Ok(snap) = clipboard::snapshot() {
            if snap.is_complete() {
                trace(&format!("  captured {}", describe_snapshot(&snap)));
                return Some(snap);
            }
            if !snap.is_empty() {
                best = Some(snap); // partial: keep it, but keep waiting
            }
        }
    }

    // Never fully filled. Keep whatever did arrive, minus the empty types.
    if let Some(partial) = best {
        let trimmed = partial.without_empty_types();
        if !trimmed.is_empty() {
            trace(&format!(
                "  captured partial (a type was declared but never filled) {}",
                describe_snapshot(&trimmed)
            ));
            return Some(trimmed);
        }
    }

    // Last chance: the app may have copied before we read the baseline, in
    // which case the counter never appears to move.
    match clipboard::snapshot() {
        Ok(snap) if !snap.is_empty() => {
            let snap = snap.without_empty_types();
            trace(&format!("  captured late {}", describe_snapshot(&snap)));
            Some(snap)
        }
        _ => None,
    }
}

/// Every UTI and payload size in a snapshot, for tracing.
fn describe_snapshot(snap: &crate::clipboard::ClipSnapshot) -> String {
    let mut parts = Vec::new();
    for (i, item) in snap.items.iter().enumerate() {
        for t in &item.types {
            parts.push(format!("i{i}:{}={}B", t.uti, t.data.len()));
        }
    }
    format!("[{}]", parts.join(" "))
}

/// One-line summary of a slot's contents for a trace.
///
/// Note this puts clipboard text in the log, so `CQ_DEBUG=1` is a debugging
/// aid, not something to leave on while handling anything secret.
fn describe(p: &crate::clipboard::SlotPreview) -> String {
    let body = match p.text.as_deref() {
        Some(t) => {
            let short: String = t.chars().take(40).collect();
            format!("{short:?}")
        }
        None if !p.files.is_empty() => format!("{} file(s): {:?}", p.files.len(), p.files),
        None => String::new(),
    };
    format!("[{} {}B] {}", p.kind, p.bytes, body)
}

/// Worker-thread-only tracing. Never call this from the tap callback.
fn trace(msg: &str) {
    if debug_on() {
        eprintln!(
            "[cq-paster] {msg}  (chord resets so far: {})",
            CHORD_RESETS.load(Ordering::Relaxed)
        );
    }
}

fn worker(
    app: AppHandle,
    state: Arc<AppState>,
    rx: mpsc::Receiver<Action>,
    injecting: Arc<AtomicBool>,
) {
    clipboard::init_thread(); // no-op on macOS; kept for symmetry with Windows

    while let Ok(action) = rx.recv() {
        match action {
            Action::Peek(slot, x, y) => {
                trace(&format!("digit {slot} armed"));
                show_popup(&app, &state, (x, y));
            }
            Action::ChordEnd => hide_popup(&app, &state),
            Action::Copy(slot) => {
                trace(&format!("COPY -> slot {slot}"));
                if clipboard::is_sensitive() {
                    trace("  skipped: pasteboard is marked concealed");
                    continue;
                }
                let Some(snap) = capture_after_copy() else {
                    // Nothing usable arrived. Leaving the slot untouched is the
                    // only safe move — overwriting it with an empty capture
                    // destroys whatever the user had saved there.
                    trace("  SKIPPED: no content published; slot left unchanged");
                    continue;
                };
                let preview = clipboard::preview(&snap);
                trace(&format!("  stored {}", describe(&preview)));
                state.folders.lock().unwrap().set(slot, snap, preview);
                state.persist();
                crate::refresh_tray(&app, &state); // fill counts changed
                refresh_state(&app, &state);
            }
            Action::Paste(slot, plain) => {
                trace(&format!("PASTE <- slot {slot} (plain={plain})"));
                let slot_snap = state.folders.lock().unwrap().get_snapshot(slot);
                let Some(slot_snap) = slot_snap else {
                    trace("  SKIPPED: slot is empty");
                    continue; // empty slot: nothing to paste
                };

                let to_paste = if plain {
                    clipboard::text_only(&slot_snap).unwrap_or(slot_snap)
                } else {
                    slot_snap
                };

                // The slot is pasted by borrowing the system pasteboard, so
                // whatever the user last copied with a plain Cmd+C has to be
                // put back afterwards. Otherwise a chord paste leaves slot N on
                // the pasteboard and the user's next plain Cmd+V re-pastes it —
                // the two pairs must stay independent.
                let borrowed = clipboard::snapshot().ok().filter(|s| !s.is_empty());
                trace(&format!(
                    "  borrowing pasteboard ({})",
                    match &borrowed {
                        Some(s) => describe(&clipboard::preview(s)),
                        None => "nothing to put back".into(),
                    }
                ));

                trace(&format!("  restoring {}", describe(&clipboard::preview(&to_paste))));
                if let Err(e) = clipboard::restore(&to_paste) {
                    trace(&format!("  SKIPPED: restore failed: {e}"));
                    continue;
                }

                injecting.store(true, Ordering::SeqCst);
                inject_paste();
                thread::sleep(Duration::from_millis(INJECT_GUARD_MS));
                injecting.store(false, Ordering::SeqCst);

                // Give the target time to actually read the pasteboard before
                // taking it back. There is no signal for "the paste completed",
                // so this one genuinely is a delay rather than a wait on an
                // event — restore too early and the target pastes the wrong
                // thing.
                if let Some(prev) = borrowed {
                    thread::sleep(PASTE_HANDBACK_DELAY);
                    match clipboard::restore(&prev) {
                        Ok(()) => trace("  pasteboard handed back"),
                        Err(e) => trace(&format!("  WARNING: could not hand back: {e}")),
                    }
                }
            }
        }
    }
}

/// Refresh frontend state and, in Noob mode, show/keep the cursor popup.
/// Push fresh state to the frontend without touching the popup's visibility.
fn refresh_state(app: &AppHandle, state: &Arc<AppState>) {
    let _ = app.emit("state-updated", state.to_dto());
}

/// Show the reference popup and keep it pinned to the cursor until the chord
/// ends.
///
/// The popup stays up for as long as Cmd is held rather than expiring on a
/// timer: it is a reference card, and it is worth reading precisely while the
/// user is still deciding which slot they want.
fn show_popup(app: &AppHandle, state: &Arc<AppState>, at: (f64, f64)) {
    refresh_state(app, state);

    if *state.mode.lock().unwrap() != Mode::Noob {
        return;
    }
    let Some(win) = app.get_webview_window("popup") else {
        return;
    };
    position_popup(&win, at);
    let _ = win.show();
    raise_popup(app);

    // One follower per chord. The generation counter cancels the previous one,
    // so a second digit does not leave two threads fighting over the position.
    let generation = state.popup_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    let state = state.clone();
    thread::spawn(move || {
        let started = std::time::Instant::now();
        while state.popup_gen.load(Ordering::SeqCst) == generation {
            if started.elapsed() > POPUP_MAX_VISIBLE {
                hide_popup(&app, &state);
                return;
            }
            if let Some(win) = app.get_webview_window("popup") {
                position_popup(&win, cursor_location());
            }
            thread::sleep(POPUP_FOLLOW);
        }
        // A newer generation owns the popup now; leave it alone.
    });
}

/// Apply the floating window style and order the popup in front.
///
/// These are raw `NSWindow` calls, and AppKit requires them on the main thread —
/// this runs on the worker, so they are marshalled rather than called directly.
/// Doing it directly terminated the process the first time a chord fired.
///
/// `run_on_main_thread` posts and returns; it does not block. That distinction
/// matters here: the blocking main-thread helpers behind Tauri's menu setters
/// deadlock when called from the main thread (MACOS_PORT.md 7), which is why
/// `refresh_tray` spawns. This is the non-blocking API and is safe from a
/// worker.
fn raise_popup(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window("popup") else {
            return;
        };
        let applied = crate::make_popup_float(&win);
        // Order in without activating: `orderFront:` can be dropped for an
        // Accessory app when the active Space is another app's full-screen
        // window, which is exactly when the popup was going missing.
        crate::order_popup_front(&win);
        match applied {
            Some((level, behavior)) => {
                trace(&format!("  popup level={level} behavior={behavior:#x}"))
            }
            None => trace("  popup: could not reach its NSWindow"),
        }
    });
}

/// Park the popup below-right of the cursor, clear of what is being worked on.
fn position_popup(win: &tauri::WebviewWindow, (x, y): (f64, f64)) {
    // CGEvent coordinates are in points, not physical pixels, so this must be a
    // LogicalPosition — a PhysicalPosition would land at double the intended
    // offset on a Retina display.
    let _ = win.set_position(tauri::LogicalPosition::new(x + 64.0, y + 100.0));
}

fn hide_popup(app: &AppHandle, state: &Arc<AppState>) {
    // Bumping the generation retires any follower still running.
    state.popup_gen.fetch_add(1, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("popup") {
        let _ = win.hide();
    }
}

/// Current pointer position, in points.
fn cursor_location() -> (f64, f64) {
    unsafe {
        let ev = CGEventCreate(std::ptr::null_mut());
        if ev.is_null() {
            return (0.0, 0.0);
        }
        let p = CGEventGetLocation(ev);
        CFRelease(ev as CFTypeRef);
        (p.x, p.y)
    }
}

fn inject_paste() {
    unsafe {
        let source = CGEventSourceCreate(KCG_EVENT_SOURCE_HID_SYSTEM_STATE);
        if source.is_null() {
            trace("  INJECT FAILED: could not create an event source");
            return;
        }
        let mut posted = 0;
        for down in [true, false] {
            let ev = CGEventCreateKeyboardEvent(source, VK_V as u16, down);
            if ev.is_null() {
                trace("  INJECT FAILED: could not create the key event");
                continue;
            }
            CGEventSetFlags(ev, KCG_FLAG_MASK_COMMAND);
            CGEventPost(KCG_HID_EVENT_TAP, ev);
            CFRelease(ev as CFTypeRef);
            posted += 1;
            // Some targets drop a key-up that arrives in the same instant as
            // its key-down; the Windows path spaces its synthetic keys too.
            thread::sleep(Duration::from_millis(5));
        }
        CFRelease(source as CFTypeRef);
        trace(&format!("  injected Cmd+V ({posted}/2 events posted)"));
    }
}

// Virtual key codes (`Events.h`).
const VK_C: i64 = 0x08;
const VK_V: i64 = 0x09;

fn is_command_key(keycode: i64) -> bool {
    matches!(keycode, 0x37 | 0x36) // left / right Command
}

/// Map a virtual key code to slot 1..=9, top row or keypad.
fn digit_of(keycode: i64) -> Option<usize> {
    match keycode {
        // Top row. Note 6 and 7 are not in numeric order in `Events.h`.
        0x12 => Some(1),
        0x13 => Some(2),
        0x14 => Some(3),
        0x15 => Some(4),
        0x17 => Some(5),
        0x16 => Some(6),
        0x1A => Some(7),
        0x1C => Some(8),
        0x19 => Some(9),
        // Numeric keypad.
        0x53 => Some(1),
        0x54 => Some(2),
        0x55 => Some(3),
        0x56 => Some(4),
        0x57 => Some(5),
        0x58 => Some(6),
        0x59 => Some(7),
        0x5B => Some(8),
        0x5C => Some(9),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_cover_both_rows() {
        for (code, want) in [(0x12, 1), (0x17, 5), (0x16, 6), (0x1A, 7), (0x19, 9)] {
            assert_eq!(digit_of(code), Some(want), "top row {code:#x}");
        }
        for (code, want) in [(0x53, 1), (0x57, 5), (0x5C, 9)] {
            assert_eq!(digit_of(code), Some(want), "keypad {code:#x}");
        }
    }

    /// Every slot 1..=9 must be reachable from both rows, with no duplicates
    /// within a row — a transposed keycode would silently target a wrong slot.
    #[test]
    fn every_slot_is_reachable_exactly_once_per_row() {
        let top: Vec<usize> = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19]
            .iter()
            .filter_map(|&c| digit_of(c))
            .collect();
        assert_eq!(top, (1..=9).collect::<Vec<_>>());

        let keypad: Vec<usize> = [0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C]
            .iter()
            .filter_map(|&c| digit_of(c))
            .collect();
        assert_eq!(keypad, (1..=9).collect::<Vec<_>>());
    }

    /// C and V must never read as digits, or a plain Cmd+C would arm a slot.
    #[test]
    fn letters_are_not_digits() {
        assert_eq!(digit_of(VK_C), None);
        assert_eq!(digit_of(VK_V), None);
        assert_eq!(digit_of(0x00), None); // kVK_ANSI_A
    }

    #[test]
    fn both_command_keys_reset_a_chord() {
        assert!(is_command_key(0x37));
        assert!(is_command_key(0x36));
        assert!(!is_command_key(VK_C));
    }

    /// The mask must cover exactly the two event types we subscribe to; the
    /// tap-disabled notifications arrive regardless of the mask.
    #[test]
    fn event_mask_covers_keydown_and_flags() {
        let mask = (1u64 << KCG_EVENT_KEY_DOWN) | (1u64 << KCG_EVENT_FLAGS_CHANGED);
        assert_ne!(mask & (1 << KCG_EVENT_KEY_DOWN), 0);
        assert_ne!(mask & (1 << KCG_EVENT_FLAGS_CHANGED), 0);
        assert_eq!(mask & (1 << 1), 0); // not mouse-down
    }
}
