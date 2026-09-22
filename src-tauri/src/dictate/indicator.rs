//! The "CQ is listening" mark, next to the pointer.
//!
//! A small bar of audio waves that moves with what the microphone is actually
//! hearing, so it is obvious both that dictation is on and that it can hear
//! you — a row of bars that animates regardless would look identical whether
//! the microphone was working or muted.
//!
//! Two rules this window has to obey, because dictation pastes into whatever
//! was focused when you started talking:
//!
//! 1. **It must never become key.** `show()` goes through
//!    `makeKeyAndOrderFront:`, which activates CQ and takes focus off the app
//!    you are dictating into — the same mistake that produced the reminder
//!    card's Dismiss bug (§7.5). It is ordered in with
//!    `orderFrontRegardless` instead, which shows a window without making it
//!    key and without activating its app.
//! 2. **It must be click-through.** `setIgnoresMouseEvents:` means a click
//!    where it happens to be sitting reaches whatever is underneath, so it
//!    cannot swallow a click on the thing you were about to type into.
//!
//! It follows the pointer while it listens, the same way the cursor popup
//! does and at the same rate — one generation counter so a second recording
//! cannot leave two threads fighting over the position, and no move at all
//! while the mouse is still, which would otherwise wake the main thread thirty
//! times a second to set the position it already has.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::NSWindow;
use tauri::{AppHandle, Emitter, Manager};

pub const WINDOW: &str = "listening";

/// Points, matching `.dl-wrap` in the stylesheet. Small enough to read as a
/// mark beside the pointer rather than a window near it.
const WIDTH: f64 = 53.0;
const HEIGHT: f64 = 19.0;
/// Below and right of the pointer, clear of the arrow itself and of what is
/// usually being pointed at.
const OFFSET: (f64, f64) = (14.0, 16.0);
/// Same rate as the cursor popup's follower: smooth to the eye, cheap.
const FOLLOW: Duration = Duration::from_millis(33);
/// Below this, a move is the hand resting rather than the mouse travelling.
const MOVED: f64 = 0.5;

/// The current input level, 0.0 to 1.0, as `f32` bits. An atomic rather than a
/// lock because it is written from the audio callback, which must not block.
static LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn set_level(v: f32) {
    LEVEL.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub fn level() -> f32 {
    f32::from_bits(LEVEL.load(Ordering::Relaxed))
}

/// Turn a block of samples into one number for the meter.
///
/// Root mean square rather than peak: peak jumps on a single click or a door
/// closing, while RMS tracks how loud speech actually is. The result is scaled
/// so ordinary speaking sits in the middle of the bar rather than at the very
/// bottom, where a meter tells you nothing.
pub fn loudness(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| {
        let v = *s as f64 / i16::MAX as f64;
        v * v
    }).sum();
    let rms = (sum / samples.len() as f64).sqrt() as f32;
    // Speech RMS lands around 0.02–0.2, so a linear meter would barely move.
    // The cube root opens up the quiet end without a full decibel scale.
    (rms.cbrt() * 1.35).clamp(0.0, 1.0)
}

pub fn build(app: &AppHandle) {
    if app.get_webview_window(WINDOW).is_some() {
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(app, WINDOW, tauri::WebviewUrl::App("index.html".into()))
        .title("CQ listening")
        .inner_size(WIDTH, HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        .build();
    if let Err(e) = built {
        crate::diag(&format!("dictate: could not create the listening mark: {e}"));
    }
}

/// Which follower owns the mark. A second recording bumps this and the
/// previous thread retires rather than fighting over the position.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn place(win: &tauri::WebviewWindow, at: (f64, f64)) {
    // Points, not pixels: a PhysicalPosition would land at double the offset
    // on a Retina display.
    let _ = win.set_position(tauri::LogicalPosition::new(at.0 + OFFSET.0, at.1 + OFFSET.1));
}

/// Show the mark at a point, given in the same point space as a CGEvent
/// location — which is what the keyboard hook has when the trigger is pressed.
pub fn show(app: &AppHandle, at: (f64, f64)) {
    build(app);
    let Some(win) = app.get_webview_window(WINDOW) else {
        return;
    };
    place(&win, at);

    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window(WINDOW) else {
            return;
        };
        let Ok(ptr) = win.ns_window() else { return };
        if ptr.is_null() {
            return;
        }
        // Safe: `run_on_main_thread` guarantees the main thread.
        let _mtm = unsafe { MainThreadMarker::new_unchecked() };
        let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
        // Clicks go through to whatever is underneath.
        ns.setIgnoresMouseEvents(true);
        // The whole point: visible, in front, and not key. `show()` would
        // activate CQ and take focus off the app being dictated into.
        ns.orderFrontRegardless();
    });
}

pub fn hide(app: &AppHandle) {
    // Retire the follower before hiding, so it cannot put the window back.
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window(WINDOW) {
        let _ = win.hide();
    }
    set_level(0.0);
}

/// Keep the mark beside the pointer and fed with the level, until the caller's
/// flag goes down or a newer recording takes over.
pub fn follow(app: AppHandle, running: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let mine = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let mut last = crate::hook::cursor_point();
        while running.load(Ordering::SeqCst) && GENERATION.load(Ordering::SeqCst) == mine {
            let at = crate::hook::cursor_point();
            // Only when it actually moved: `set_position` is marshalled to the
            // main thread, and doing that thirty times a second to set the
            // position it already has is work for nothing.
            if (at.0 - last.0).abs() > MOVED || (at.1 - last.1).abs() > MOVED {
                if let Some(win) = app.get_webview_window(WINDOW) {
                    place(&win, at);
                }
                last = at;
            }
            let _ = app.emit_to(WINDOW, "dictate-level", level());
            std::thread::sleep(FOLLOW);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_reads_as_nothing() {
        assert_eq!(loudness(&[]), 0.0);
        assert_eq!(loudness(&[0; 512]), 0.0);
    }

    #[test]
    fn louder_input_reads_higher() {
        let quiet: Vec<i16> = (0..512).map(|i| ((i % 32) as i16) * 4).collect();
        let loud: Vec<i16> = (0..512).map(|i| ((i % 32) as i16) * 400).collect();
        assert!(loudness(&loud) > loudness(&quiet));
    }

    #[test]
    fn the_meter_never_leaves_its_bar() {
        // Full-scale input, including the negative rail, must not exceed 1.0 —
        // a bar wider than its track draws outside the window.
        assert!(loudness(&[i16::MAX; 256]) <= 1.0);
        assert!(loudness(&[i16::MIN + 1; 256]) <= 1.0);
        assert!(loudness(&[i16::MAX; 256]) > 0.9, "full scale should peg the meter");
    }

    #[test]
    fn ordinary_speech_lands_in_the_middle_of_the_bar() {
        // RMS around 0.05 is normal speaking level. On a linear meter that
        // would be 5% and look broken.
        let speech: Vec<i16> = (0..1024)
            .map(|i| ((i as f32 * 0.1).sin() * 0.05 * i16::MAX as f32) as i16)
            .collect();
        let l = loudness(&speech);
        assert!(l > 0.3 && l < 0.9, "speech read as {l}, which is off the useful part of the meter");
    }

    #[test]
    fn the_level_survives_a_round_trip() {
        set_level(0.42);
        assert!((level() - 0.42).abs() < 0.0001);
        // Out-of-range input is clamped rather than stored.
        set_level(5.0);
        assert_eq!(level(), 1.0);
        set_level(-1.0);
        assert_eq!(level(), 0.0);
    }
}
