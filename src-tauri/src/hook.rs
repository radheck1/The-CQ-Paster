//! The global low-level keyboard hook and the chord state machine.
//!
//! Chord scheme (digit BEFORE letter so we know the slot before the action
//! fires):
//!   * `Ctrl + <N> + C`  -> copy the current selection into slot N
//!   * `Ctrl + <N> + V`  -> paste slot N
//!
//! While Ctrl is held, tapping 1..9 is swallowed (the app owns Ctrl+digit as
//! its trigger prefix). Plain `Ctrl+C` / `Ctrl+V` with no digit pass straight
//! through, so normal copy/paste is untouched.

#[cfg(windows)]
pub fn start(app: tauri::AppHandle, state: std::sync::Arc<crate::AppState>) {
    windows_impl::start(app, state);
}

#[cfg(not(windows))]
pub fn start(_app: tauri::AppHandle, _state: std::sync::Arc<crate::AppState>) {
    // Hook backend for non-Windows platforms is not implemented yet.
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use tauri::{AppHandle, Emitter, Manager};

    use crate::clipboard;
    use crate::{AppState, Mode};

    /// How long to wait after letting `Ctrl+C` through before reading the
    /// clipboard, so the foreground app has finished populating it.
    const COPY_SETTLE_MS: u64 = 120;
    /// How long to leave the "injecting" guard up so our synthetic keystrokes
    /// pass through the hook untouched.
    const INJECT_GUARD_MS: u64 = 40;
    /// Popup auto-hide delay after the last chord activity.
    const POPUP_HIDE_MS: u64 = 2200;

    enum Action {
        /// Ctrl+digit pressed: show the reference popup (Noob mode).
        Peek(i32, i32),
        /// Ctrl+<N>+C: copy current selection into slot N.
        Copy(usize),
        /// Ctrl+<N>+V: paste slot N. `plain` (Shift held) strips formatting.
        Paste(usize, bool),
    }

    pub fn start(app: AppHandle, state: Arc<AppState>) {
        let (tx, rx) = mpsc::channel::<Action>();
        let injecting = Arc::new(AtomicBool::new(false));

        // Worker: does the slow clipboard work off the hook thread.
        {
            let injecting = injecting.clone();
            thread::spawn(move || worker(app, state, rx, injecting));
        }

        // Hook thread: installs the low-level grab and runs its message loop.
        {
            let injecting = injecting.clone();
            thread::spawn(move || run_grab(tx, injecting));
        }
    }

    fn run_grab(tx: Sender<Action>, injecting: Arc<AtomicBool>) {
        use rdev::{Event, EventType, Key};
        use std::sync::atomic::AtomicUsize;

        // The only cached chord state is which digit was pressed; 0 = none.
        //
        // We deliberately do NOT cache the Ctrl state — it's queried live per
        // keypress. A Ctrl release can be missed while we're injecting a paste,
        // and a cached flag would then go stale and misread a later plain Ctrl+C
        // as a slot store, clobbering a saved slot.
        let pending_slot = AtomicUsize::new(0);

        // IMPORTANT: keep this callback minimal and non-blocking. A low-level
        // keyboard hook that takes too long (~300ms, LowLevelHooksTimeout) is
        // silently bypassed by Windows, which lets suppressed keys leak through.
        // Do NOT add logging or slow work here.
        let callback = move |event: Event| -> Option<Event> {
            // Let our own synthetic keystrokes through without reprocessing.
            if injecting.load(Ordering::SeqCst) {
                return Some(event);
            }

            if let EventType::KeyPress(key) = event.event_type {
                // A fresh Ctrl press begins a new chord: forget any leftover
                // digit so a plain Ctrl+C / Ctrl+V never reuses a stale slot.
                if is_ctrl(key) {
                    pending_slot.store(0, Ordering::SeqCst);
                    return Some(event);
                }
                // Only act while Ctrl is physically held (queried live).
                if !ctrl_physically_down() {
                    return Some(event);
                }
                if let Some(d) = digit_of(key) {
                    pending_slot.store(d, Ordering::SeqCst);
                    let (x, y) = cursor_pos();
                    let _ = tx.send(Action::Peek(x, y));
                    return None; // swallow the digit
                }
                match key {
                    Key::KeyC => {
                        let slot = pending_slot.load(Ordering::SeqCst);
                        if slot != 0 {
                            let _ = tx.send(Action::Copy(slot));
                        }
                        return Some(event); // let Ctrl+C perform the real copy
                    }
                    Key::KeyV => {
                        let slot = pending_slot.load(Ordering::SeqCst);
                        if slot != 0 {
                            let plain = shift_physically_down(); // Shift → plain text
                            let _ = tx.send(Action::Paste(slot, plain));
                            return None; // swallow V; worker injects paste
                        }
                        return Some(event);
                    }
                    _ => {}
                }
            }
            Some(event)
        };

        if let Err(err) = rdev::grab(callback) {
            eprintln!("[cq-paster] keyboard grab failed: {err:?}");
        }
    }

    fn worker(
        app: AppHandle,
        state: Arc<AppState>,
        rx: mpsc::Receiver<Action>,
        injecting: Arc<AtomicBool>,
    ) {
        // Shell path resolution (SHGetPathFromIDListW) needs COM on this thread.
        clipboard::init_thread();

        while let Ok(action) = rx.recv() {
            match action {
                Action::Peek(x, y) => {
                    show_activity(&app, &state, Some((x, y)));
                }
                Action::Copy(slot) => {
                    thread::sleep(Duration::from_millis(COPY_SETTLE_MS));
                    // Respect apps that flag their content as sensitive /
                    // exclude-from-history (password managers, banking, etc.).
                    if clipboard::is_sensitive() {
                        continue;
                    }
                    match clipboard::snapshot() {
                        Ok(snap) if !snap.is_empty() => {
                            let preview = clipboard::preview(&snap);
                            state.slots.lock().unwrap().set(slot, snap, preview);
                            state.persist();
                            show_activity(&app, &state, None);
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[cq-paster] copy snapshot failed: {e}"),
                    }

                }
                Action::Paste(slot, plain) => {
                    let slot_snap = state.slots.lock().unwrap().get_snapshot(slot);
                    let Some(slot_snap) = slot_snap else {
                        continue; // empty slot: nothing to paste
                    };

                    // Plain-text paste: strip everything but the text formats so
                    // the target pastes unformatted. Falls back to the full slot
                    // if it holds no text (e.g. an image).
                    let to_paste = if plain {
                        clipboard::text_only(&slot_snap).unwrap_or(slot_snap)
                    } else {
                        slot_snap
                    };

                    // Load the slot onto the clipboard, then inject a paste. We
                    // intentionally do NOT snapshot the live clipboard first:
                    // reading a source app's asynchronous data object can make it
                    // re-assert the clipboard and clobber what we set.
                    if clipboard::restore(&to_paste).is_err() {
                        continue;
                    }

                    injecting.store(true, Ordering::SeqCst);
                    // `plain` means Shift is physically held; release it during
                    // injection so the target gets a clean Ctrl+V, not Ctrl+Shift+V.
                    inject_paste(plain);
                    thread::sleep(Duration::from_millis(INJECT_GUARD_MS));
                    injecting.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    /// Refresh frontend state and, in Noob mode, show/keep the cursor popup.
    fn show_activity(app: &AppHandle, state: &Arc<AppState>, at: Option<(i32, i32)>) {
        let _ = app.emit("state-updated", state.to_dto());

        if *state.mode.lock().unwrap() != Mode::Noob {
            return;
        }
        if let Some(win) = app.get_webview_window("popup") {
            if let Some((x, y)) = at {
                // Offset the popup down ~an inch and to the right of the cursor
                // so it doesn't sit on top of what the user is working on.
                let _ = win.set_position(tauri::PhysicalPosition::new(x + 64, y + 100));
            }
            let _ = win.show();
        }

        // Reset the auto-hide timer using a generation counter.
        let gen = state.popup_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let app = app.clone();
        let state = state.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(POPUP_HIDE_MS));
            if state.popup_gen.load(Ordering::SeqCst) == gen {
                if let Some(win) = app.get_webview_window("popup") {
                    let _ = win.hide();
                }
            }
        });
    }

    fn inject_paste(release_shift: bool) {
        use rdev::{simulate, EventType, Key};

        // For a plain-text paste the user is holding Shift; release it so the
        // target receives Ctrl+V (not Ctrl+Shift+V, which triggers paste-special
        // dialogs in some apps). The clipboard already holds text-only.
        if release_shift {
            let _ = simulate(&EventType::KeyRelease(Key::ShiftLeft));
            let _ = simulate(&EventType::KeyRelease(Key::ShiftRight));
            thread::sleep(Duration::from_millis(5));
        }

        // If the user is still physically holding Ctrl (the common case), just
        // tap V — the OS sees Ctrl+V. Otherwise wrap our own Ctrl around it.
        let wrap = !ctrl_physically_down();
        if wrap {
            let _ = simulate(&EventType::KeyPress(Key::ControlLeft));
            thread::sleep(Duration::from_millis(5));
        }
        let _ = simulate(&EventType::KeyPress(Key::KeyV));
        let _ = simulate(&EventType::KeyRelease(Key::KeyV));
        if wrap {
            thread::sleep(Duration::from_millis(5));
            let _ = simulate(&EventType::KeyRelease(Key::ControlLeft));
        }
    }

    fn is_ctrl(key: rdev::Key) -> bool {
        matches!(key, rdev::Key::ControlLeft | rdev::Key::ControlRight)
    }

    fn digit_of(key: rdev::Key) -> Option<usize> {
        use rdev::Key::*;
        match key {
            // Top-row number keys.
            Num1 => Some(1),
            Num2 => Some(2),
            Num3 => Some(3),
            Num4 => Some(4),
            Num5 => Some(5),
            Num6 => Some(6),
            Num7 => Some(7),
            Num8 => Some(8),
            Num9 => Some(9),
            // Numeric keypad (works regardless of Num Lock).
            Kp1 => Some(1),
            Kp2 => Some(2),
            Kp3 => Some(3),
            Kp4 => Some(4),
            Kp5 => Some(5),
            Kp6 => Some(6),
            Kp7 => Some(7),
            Kp8 => Some(8),
            Kp9 => Some(9),
            _ => None,
        }
    }

    fn cursor_pos() -> (i32, i32) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }

    fn ctrl_physically_down() -> bool {
        key_down(0x11) // VK_CONTROL
    }

    fn shift_physically_down() -> bool {
        key_down(0x10) // VK_SHIFT
    }

    fn key_down(vk: i32) -> bool {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
    }
}
