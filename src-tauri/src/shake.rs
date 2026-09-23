//! Open the control panel by shaking the mouse (macOS only).
//!
//! A shake is a run of fast movements that turn back on themselves. The numbers
//! below come from a recording of real use rather than guesswork: a deliberate
//! shake makes 9 or 10 such turns inside 0.6 s, while the busiest second of
//! ordinary mouse work made 2. The trigger sits in that gap, and because the gap
//! is wide, the exact speed threshold barely matters — every value between 900
//! and 3200 points per second gave the same answer on the recording.
//!
//! **The events come from their own tap, deliberately not the keyboard's.** A
//! mouse can deliver a thousand events a second, and the keyboard tap is the one
//! that suppresses the chord keys: if it ever runs slow, macOS disables it and
//! the hotkeys stop working (§5.1). This tap is listen-only, on its own thread,
//! so the two can't take each other down. The callback only does arithmetic and,
//! on a shake, sends one message; the window is opened from the worker thread.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

/// How long a shake has to happen within.
const WINDOW: f64 = 0.6;
/// How far the pointer travels in that window, in points: a shake covers
/// thousands, so this only rules out a twitch that happens to change direction.
const MIN_PATH: f64 = 600.0;
/// Quiet period after firing, so one shake opens the window once.
const COOLDOWN: f64 = 2.0;
/// Two fast strokes count as a turn when the angle between them is wider than
/// 135°: a shake doubles back on itself, an arc doesn't.
const TURN_COS: f64 = -0.7071;

// ---- Settings ------------------------------------------------------------------------

/// How hard the gesture has to be. On the recording, a real shake cleared even
/// `Low` about four times over, and ordinary use reached half of `High`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Low,
    Medium,
    High,
}

impl Sensitivity {
    const ALL: [Sensitivity; 3] = [Sensitivity::Low, Sensitivity::Medium, Sensitivity::High];

    fn thresholds(self) -> Thresholds {
        match self {
            Sensitivity::Low => Thresholds { turns: 5, speed: 1800.0 },
            Sensitivity::Medium => Thresholds { turns: 4, speed: 1300.0 },
            // Four turns, not three: three at this speed fired once on the
            // recording of ordinary use, which is the one thing it must not do.
            Sensitivity::High => Thresholds { turns: 4, speed: 900.0 },
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Sensitivity::Low => "low",
            Sensitivity::Medium => "medium",
            Sensitivity::High => "high",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Sensitivity::ALL.into_iter().find(|s| s.key() == key)
    }
}

struct Thresholds {
    turns: usize,
    speed: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub on: bool,
    pub sensitivity: Sensitivity,
}

impl Default for Settings {
    fn default() -> Self {
        // On by default: this exists to be discovered by shaking the mouse.
        Settings { on: true, sensitivity: Sensitivity::Medium }
    }
}

/// Read by the tap callback on every event, so they are atomics rather than a
/// lock: a callback that waits on a mutex is a callback that can run slow.
static ENABLED: AtomicBool = AtomicBool::new(false);
static SENSITIVITY: AtomicUsize = AtomicUsize::new(1);
/// The live tap, so the menu can switch it off without tearing anything down.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn file() -> std::path::PathBuf {
    crate::data_dir().join("shake.json")
}

pub fn settings() -> Settings {
    Settings {
        on: ENABLED.load(Ordering::Relaxed),
        sensitivity: Sensitivity::ALL[SENSITIVITY.load(Ordering::Relaxed).min(2)],
    }
}

/// Apply new settings, save them, and switch the tap on or off to match.
pub fn set_settings(next: Settings) {
    ENABLED.store(next.on, Ordering::Relaxed);
    let index = Sensitivity::ALL.iter().position(|s| *s == next.sensitivity).unwrap_or(1);
    SENSITIVITY.store(index, Ordering::Relaxed);
    let port = TAP_PORT.load(Ordering::SeqCst);
    if !port.is_null() {
        // Off means no events delivered at all, rather than a callback that
        // returns early: nothing to run, nothing to get slow.
        unsafe { CGEventTapEnable(port, next.on) };
    }
    match serde_json::to_string(&next) {
        Ok(json) => {
            if let Err(e) = crate::jotter::save(&file(), &json) {
                crate::diag(&format!("shake: could not save the setting: {e}"));
            }
        }
        Err(e) => crate::diag(&format!("shake: could not write the setting: {e}")),
    }
}

pub fn set_sensitivity(key: &str) {
    if let Some(sensitivity) = Sensitivity::from_key(key) {
        set_settings(Settings { sensitivity, ..settings() });
    }
}

pub fn toggle() {
    let now = settings();
    set_settings(Settings { on: !now.on, ..now });
}

// ---- The detector --------------------------------------------------------------------

struct Detector {
    /// Recent samples: seconds, x, y.
    recent: VecDeque<(f64, f64, f64)>,
    /// When each turn happened, within the window.
    turns: VecDeque<f64>,
    /// The direction of the last stroke fast enough to count.
    heading: Option<(f64, f64)>,
    last_fire: f64,
}

impl Default for Detector {
    fn default() -> Self {
        Detector {
            recent: VecDeque::new(),
            turns: VecDeque::new(),
            heading: None,
            // Not zero: that would put the cooldown over the first two seconds
            // after the tap starts, so a shake right after launch would do
            // nothing at all.
            last_fire: f64::NEG_INFINITY,
        }
    }
}

impl Detector {
    /// Feed one pointer sample. True when this sample completes a shake.
    fn push(&mut self, t: f64, x: f64, y: f64, held: bool, k: &Thresholds) -> bool {
        if held {
            // Throwing a window across the screen is fast and can double back;
            // it is not a request to open anything.
            self.clear();
            return false;
        }
        if let Some(&(last_t, _, _)) = self.recent.back() {
            let dt = (t - last_t).max(1e-4);
            let (dx, dy) = (x - self.recent.back().unwrap().1, y - self.recent.back().unwrap().2);
            let dist = dx.hypot(dy);
            if dist / dt > k.speed {
                let heading = (dx / dist, dy / dist);
                if let Some(before) = self.heading {
                    if before.0 * heading.0 + before.1 * heading.1 < TURN_COS {
                        self.turns.push_back(t);
                    }
                }
                self.heading = Some(heading);
            }
        }
        self.recent.push_back((t, x, y));
        while self.recent.front().is_some_and(|s| t - s.0 > WINDOW) {
            self.recent.pop_front();
        }
        while self.turns.front().is_some_and(|u| t - u > WINDOW) {
            self.turns.pop_front();
        }
        if self.turns.len() < k.turns || t - self.last_fire <= COOLDOWN {
            return false;
        }
        let path: f64 = self
            .recent
            .iter()
            .zip(self.recent.iter().skip(1))
            .map(|(a, b)| (b.1 - a.1).hypot(b.2 - a.2))
            .sum();
        if path < MIN_PATH {
            return false;
        }
        self.last_fire = t;
        self.turns.clear();
        true
    }

    fn clear(&mut self) {
        self.recent.clear();
        self.turns.clear();
        self.heading = None;
    }
}

// ---- The tap -------------------------------------------------------------------------

// The same raw FFI approach as the keyboard hook (§4.2), declared here rather
// than shared so that file — the one that must never be made slower — keeps to
// itself.
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;

const KCG_EVENT_MOUSE_MOVED: u32 = 5;
const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const KCG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
const KCG_SESSION_EVENT_TAP: u32 = 1;
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
/// Listen-only: this tap reads the mouse, it never swallows anything.
const KCG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(port: *mut c_void, enable: bool);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(allocator: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

/// Everything the callback reaches through the tap's `user_info`. Leaked on
/// purpose: it outlives the tap, which runs for the life of the process.
struct Ctx {
    detector: Detector,
    tx: Sender<()>,
    started: Instant,
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if event_type == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        let port = TAP_PORT.load(Ordering::SeqCst);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, ENABLED.load(Ordering::Relaxed)) };
        }
        return event;
    }
    if user_info.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return event;
    }
    // Safe: the context is leaked, and only this callback touches it — the tap
    // delivers on one thread, its own run loop.
    let ctx = unsafe { &mut *(user_info as *mut Ctx) };
    let at = unsafe { CGEventGetLocation(event) };
    let k = Sensitivity::ALL[SENSITIVITY.load(Ordering::Relaxed).min(2)].thresholds();
    let held = event_type != KCG_EVENT_MOUSE_MOVED;
    let t = ctx.started.elapsed().as_secs_f64();
    if ctx.detector.push(t, at.x, at.y, held, &k) {
        // One message, never any work here: opening a window from a tap
        // callback is exactly the kind of thing that gets a tap disabled.
        let _ = ctx.tx.send(());
    }
    event
}

/// Read the saved setting, start the worker, and install the tap.
pub fn start(app: &AppHandle) {
    let saved = std::fs::read_to_string(file())
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default();
    ENABLED.store(saved.on, Ordering::Relaxed);
    SENSITIVITY.store(
        Sensitivity::ALL.iter().position(|s| *s == saved.sensitivity).unwrap_or(1),
        Ordering::Relaxed,
    );

    let (tx, rx) = mpsc::channel::<()>();
    let handle = app.clone();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            crate::diag("shake: opening the control panel");
            // Where the shake happened, not where the window was left.
            crate::show_main_at_pointer(&handle);
        }
    });
    std::thread::spawn(move || run_tap(tx));
}

fn run_tap(tx: Sender<()>) {
    let ctx = Box::into_raw(Box::new(Ctx { detector: Detector::default(), tx, started: Instant::now() }));
    let mask = (1u64 << KCG_EVENT_MOUSE_MOVED)
        | (1u64 << KCG_EVENT_LEFT_MOUSE_DRAGGED)
        | (1u64 << KCG_EVENT_RIGHT_MOUSE_DRAGGED)
        | (1u64 << KCG_EVENT_OTHER_MOUSE_DRAGGED);
    let port = unsafe {
        CGEventTapCreate(
            KCG_SESSION_EVENT_TAP,
            KCG_HEAD_INSERT_EVENT_TAP,
            KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
            mask,
            tap_callback,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        // The same permissions as the keyboard tap, which asks for them at
        // startup; nothing more to ask for here.
        crate::diag("shake: could not create the mouse tap (permissions?)");
        return;
    }
    TAP_PORT.store(port, Ordering::SeqCst);
    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CFRelease(source);
        CGEventTapEnable(port, ENABLED.load(Ordering::Relaxed));
        crate::diag(&format!(
            "shake: watching the mouse, {} at {} sensitivity",
            if ENABLED.load(Ordering::Relaxed) { "on" } else { "off" },
            settings().sensitivity.key()
        ));
        CFRunLoopRun();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay a recording of the pointer. Returns how many times it fired.
    fn replay(trace: &str, sensitivity: Sensitivity, held: bool) -> usize {
        let k = sensitivity.thresholds();
        let mut detector = Detector::default();
        let mut fires = 0;
        for line in trace.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace().map(|p| p.parse::<f64>().unwrap());
            let (t, x, y) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
            if detector.push(t, x, y, held, &k) {
                fires += 1;
            }
        }
        fires
    }

    /// A pointer path as a trace: `points` sampled every `step` seconds.
    fn trace(points: impl Iterator<Item = (f64, f64)>, step: f64) -> String {
        points
            .enumerate()
            .map(|(i, (x, y))| format!("{:.3} {x:.1} {y:.1}", i as f64 * step))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Side to side `times` times, `span` points wide, one sample every 10 ms.
    fn zigzag(times: usize, span: f64, samples_per_leg: usize) -> String {
        let mut points = Vec::new();
        for leg in 0..times {
            for i in 0..samples_per_leg {
                let along = i as f64 / samples_per_leg as f64;
                let x = if leg % 2 == 0 { along * span } else { span - along * span };
                points.push((x, 400.0));
            }
        }
        trace(points.into_iter(), 0.01)
    }

    const SHAKE: &str = include_str!("testdata/shake.trace");
    const ORDINARY: &str = include_str!("testdata/normal.trace");

    #[test]
    fn a_recorded_shake_opens_the_window() {
        for sensitivity in Sensitivity::ALL {
            assert!(
                replay(SHAKE, sensitivity, false) >= 1,
                "a real shake should fire at {} sensitivity",
                sensitivity.key()
            );
        }
    }

    #[test]
    fn recorded_ordinary_use_never_fires() {
        for sensitivity in Sensitivity::ALL {
            assert_eq!(
                replay(ORDINARY, sensitivity, false),
                0,
                "ordinary mouse use fired at {} sensitivity",
                sensitivity.key()
            );
        }
    }

    #[test]
    fn dragging_is_never_a_shake() {
        assert_eq!(replay(SHAKE, Sensitivity::High, true), 0);
    }

    #[test]
    fn the_cooldown_keeps_one_shake_to_one_window() {
        // The recorded shake runs for 3 s, well past the 2 s cooldown, so it is
        // allowed to fire again — but not more often than the cooldown.
        let fires = replay(SHAKE, Sensitivity::High, false);
        assert!((1..=2).contains(&fires), "fired {fires} times");
    }

    #[test]
    fn a_fast_straight_sweep_is_not_a_shake() {
        // 4000 points per second across the screen, never turning back.
        let sweep = trace((0..120).map(|i| (i as f64 * 40.0, 500.0)), 0.01);
        assert_eq!(replay(&sweep, Sensitivity::High, false), 0);
    }

    #[test]
    fn a_slow_wiggle_is_not_a_shake() {
        // The same shape as a shake at a tenth of the speed: 20 points a leg.
        assert_eq!(replay(&zigzag(8, 20.0, 10), Sensitivity::High, false), 0);
    }

    #[test]
    fn sensitivity_is_ordered() {
        // A shake at about 1100 points per second: above what high asks for,
        // below medium and low.
        let gentle = zigzag(7, 110.0, 10);
        assert!(replay(&gentle, Sensitivity::High, false) >= 1, "high should catch it");
        assert_eq!(replay(&gentle, Sensitivity::Medium, false), 0, "medium should not");
        assert_eq!(replay(&gentle, Sensitivity::Low, false), 0, "low should not");
    }

    #[test]
    fn settings_round_trip_through_json() {
        let json = serde_json::to_string(&Settings { on: false, sensitivity: Sensitivity::Low }).unwrap();
        assert!(json.contains("\"low\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(!back.on && back.sensitivity == Sensitivity::Low);
        assert_eq!(Sensitivity::from_key("medium"), Some(Sensitivity::Medium));
        assert_eq!(Sensitivity::from_key("nonsense"), None);
    }
}
