//! Jotter reminders (macOS only).
//!
//! Each Jotter folder can remind you about its open lines on a schedule: every
//! so many minutes, lined up with the clock, between a start and an end time on
//! chosen days. A reminder is a CQ card in the top-right corner of the screen
//! the pointer is on, with the folder's sound and an orange dot on the menu-bar
//! icon. The card stays until it is dealt with.
//!
//! The settings and the notes live in the Jotter document, which the frontend
//! owns and saves through `jotter_save`. This module keeps a parsed copy of the
//! parts it needs, refreshed on every save, and runs the schedule on its own
//! thread: the control panel's web view is hidden most of the time, and a hidden
//! web view's timers are throttled.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use objc2::runtime::NSObjectProtocol;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBox, NSBoxType, NSColor, NSSound, NSTitlePosition};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// How often the schedule is checked. Well under `GRACE_SECS`, so no reminder
/// is missed while the Mac is awake.
const TICK: Duration = Duration::from_secs(15);
/// A reminder found later than this is skipped rather than shown late: the Mac
/// slept through it, and a pile of stale reminders on wake helps nobody.
const GRACE_SECS: i64 = 90;
const SNOOZE_MINUTES: i64 = 10;
/// Open lines listed per folder on the card.
const CARD_ITEMS: usize = 3;
/// The card's width and its gap from the screen edges, in points.
const CARD_WIDTH: f64 = 320.0;
const CARD_MARGIN: f64 = 12.0;
const CARD_LABEL: &str = "reminder";
const TRAY_ID: &str = "cq-tray";

// ---- The Jotter document, as far as reminders care ----------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct Doc {
    folders: Vec<Folder>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Folder {
    id: u64,
    name: String,
    lines: Vec<Line>,
    reminder: Settings,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Line {
    text: String,
    depth: u32,
    done: bool,
}

/// One folder's reminder settings, as `jotter.ts` writes them. Fields only the
/// settings menu uses are ignored here.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
struct Settings {
    on: bool,
    /// Minutes between reminders, counted from midnight so they land on the clock.
    every: u32,
    /// Minutes after midnight, both ends included. `start > end` runs past midnight.
    start: u32,
    end: u32,
    /// 0 = Sunday … 6 = Saturday, as JavaScript's `getDay()` counts.
    days: Vec<u32>,
    /// A macOS system sound, or empty for none.
    sound: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on: false,
            every: 60,
            start: 9 * 60,
            end: 17 * 60,
            days: vec![1, 2, 3, 4, 5],
            sound: "Glass".into(),
        }
    }
}

// ---- The schedule --------------------------------------------------------------
//
// Pure functions of a local time, so they can be tested without a clock.

fn every(s: &Settings) -> u32 {
    s.every.clamp(5, 720)
}

fn minute_of(t: NaiveDateTime) -> u32 {
    t.hour() * 60 + t.minute()
}

/// Whether a time on the reminder grid falls inside the window.
fn in_window(s: &Settings, t: NaiveDateTime) -> bool {
    let minute = minute_of(t);
    let today = t.weekday().num_days_from_sunday();
    if s.start <= s.end {
        (s.start..=s.end).contains(&minute) && s.days.contains(&today)
    } else if minute >= s.start {
        s.days.contains(&today)
    } else if minute <= s.end {
        // The small hours of a window that opened the evening before.
        s.days.contains(&((today + 6) % 7))
    } else {
        false
    }
}

/// The latest grid time at or before `now`, if the window includes it.
fn latest_slot(s: &Settings, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let minute = minute_of(now);
    let slot = minute - minute % every(s);
    let t = now.date().and_hms_opt(slot / 60, slot % 60, 0)?;
    in_window(s, t).then_some(t)
}

/// The first grid time after `now` that the window includes, within a week.
fn next_slot(s: &Settings, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let mut day = now.date();
    for _ in 0..8 {
        for minute in (0..24 * 60).step_by(every(s) as usize) {
            let t = day.and_hms_opt(minute / 60, minute % 60, 0)?;
            if t > now && in_window(s, t) {
                return Some(t);
            }
        }
        day = day.succ_opt()?;
    }
    None
}

/// The lines with text that aren't crossed out, by themselves or by a line they
/// sit under — the same rule as `crossings` in `outline.ts`.
fn open_lines(lines: &[Line]) -> Vec<&Line> {
    let mut above: Vec<(u32, bool)> = Vec::new();
    let mut open = Vec::new();
    for line in lines {
        while above.last().is_some_and(|&(depth, _)| depth >= line.depth) {
            above.pop();
        }
        let crossed = line.done || above.last().is_some_and(|&(_, c)| c);
        above.push((line.depth, crossed));
        if !crossed && !line.text.trim().is_empty() {
            open.push(line);
        }
    }
    open
}

fn has_open(lines: &[Line]) -> bool {
    !open_lines(lines).is_empty()
}

// ---- State ---------------------------------------------------------------------

#[derive(Default)]
struct State {
    doc: Doc,
    /// The grid time each folder last reminded at, so each time fires once.
    fired: HashMap<u64, NaiveDateTime>,
    /// Snoozed folders, and until when.
    snoozed: HashMap<u64, NaiveDateTime>,
    /// Folders on the card right now, in the order they arrived.
    showing: Vec<u64>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The folders due at `now`, recording what fired.
fn due_folders(st: &mut State, now: NaiveDateTime) -> Vec<u64> {
    let State { doc, fired, snoozed, .. } = st;
    let mut due = Vec::new();
    for f in &doc.folders {
        let slot = latest_slot(&f.reminder, now);
        if let Some(until) = snoozed.get(&f.id).copied() {
            // The regular schedule waits while a snooze is pending, and the time
            // it lands on is spent either way.
            if let Some(t) = slot {
                fired.insert(f.id, t);
            }
            if now < until {
                continue;
            }
            snoozed.remove(&f.id);
            if f.reminder.on && has_open(&f.lines) {
                due.push(f.id);
            }
            continue;
        }
        let Some(t) = slot else { continue };
        if !f.reminder.on || fired.get(&f.id) == Some(&t) || (now - t).num_seconds() > GRACE_SECS {
            continue;
        }
        fired.insert(f.id, t);
        // Nothing open, nothing to remind about.
        if has_open(&f.lines) {
            due.push(f.id);
        }
    }
    due
}

// ---- The card --------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct Card {
    folders: Vec<CardFolder>,
}

#[derive(Serialize, Clone)]
struct CardFolder {
    id: u64,
    name: String,
    items: Vec<CardItem>,
    /// Open lines beyond the ones listed.
    more: usize,
}

#[derive(Serialize, Clone)]
struct CardItem {
    text: String,
    depth: u32,
}

fn build_card(doc: &Doc, ids: &[u64]) -> Card {
    let folders = ids
        .iter()
        .filter_map(|id| doc.folders.iter().find(|f| f.id == *id))
        .map(|f| {
            let open = open_lines(&f.lines);
            CardFolder {
                id: f.id,
                name: f.name.clone(),
                items: open
                    .iter()
                    .take(CARD_ITEMS)
                    .map(|l| CardItem { text: l.text.trim().to_string(), depth: l.depth })
                    .collect(),
                more: open.len().saturating_sub(CARD_ITEMS),
            }
        })
        .collect();
    Card { folders }
}

// ---- Running it --------------------------------------------------------------------

/// Read the saved document, create the card's window, and start the schedule.
pub fn start(app: &AppHandle) {
    // Unreadable or missing is the frontend's to deal with; the first save
    // brings this copy up to date either way.
    if let Ok(text) = std::fs::read_to_string(crate::jotter::file()) {
        if let Ok(doc) = serde_json::from_str::<Doc>(&text) {
            state().doc = doc;
        }
    }

    let built = tauri::WebviewWindowBuilder::new(app, CARD_LABEL, tauri::WebviewUrl::App("index.html".into()))
        .title("CQ Jotter reminder")
        .inner_size(CARD_WIDTH, 200.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        // A click on the card should press the button under it, not merely
        // bring the window forward.
        .accept_first_mouse(true)
        .build();
    if let Err(e) = built {
        crate::diag(&format!("reminder: could not create the card window: {e}"));
    }

    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        let due = due_folders(&mut state(), Local::now().naive_local());
        if !due.is_empty() {
            crate::diag(&format!("reminder: {} folder(s) due", due.len()));
            show(&app, &due, true);
        }
    });
}

/// Take in a document the frontend has just saved.
pub fn update_doc(app: &AppHandle, json: &str) {
    let Ok(doc) = serde_json::from_str::<Doc>(json) else {
        crate::diag("reminder: could not read the saved document");
        return;
    };
    let now = Local::now().naive_local();
    let card_up = {
        let mut st = state();
        for f in &doc.folders {
            let was_on = st.doc.folders.iter().any(|old| old.id == f.id && old.reminder.on);
            if f.reminder.on && !was_on {
                // Switching reminders on starts with the next time on the clock,
                // not with one for the time that just went by.
                if let Some(t) = latest_slot(&f.reminder, now) {
                    st.fired.insert(f.id, t);
                }
            }
        }
        st.doc = doc;
        let State { doc, fired, snoozed, showing } = &mut *st;
        let exists = |id: &u64| doc.folders.iter().any(|f| f.id == *id);
        fired.retain(|id, _| exists(id));
        snoozed.retain(|id, _| exists(id));
        showing.retain(exists);
        !showing.is_empty()
    };
    // Keep a card that's up in step with the note: crossing a line out takes it
    // off the card, and crossing out the last one takes the card away.
    if card_up {
        show(app, &[], false);
    }
}

/// Put folders on the card, merged with any already there, and hand it to the
/// card's window to draw. With `alert`, this is a new reminder: play the sound
/// and set the menu-bar dot. Without, it's a refresh.
fn show(app: &AppHandle, ids: &[u64], alert: bool) {
    let (card, sound) = {
        let mut st = state();
        let State { doc, showing, .. } = &mut *st;
        for id in ids {
            if !showing.contains(id) {
                showing.push(*id);
            }
        }
        if !alert {
            showing.retain(|id| doc.folders.iter().any(|f| f.id == *id && has_open(&f.lines)));
        }
        let sound = ids
            .iter()
            .filter_map(|id| doc.folders.iter().find(|f| f.id == *id))
            .map(|f| f.reminder.sound.clone())
            .find(|s| !s.is_empty());
        (build_card(doc, showing), sound)
    };
    if card.folders.is_empty() {
        hide(app);
        return;
    }
    // The window draws it and calls back with its height (`reminder_present`).
    let _ = app.emit_to(CARD_LABEL, "reminder-card", card);
    if alert {
        if let Some(name) = sound {
            play(app, name);
        }
        set_badge(app, true);
    }
}

fn hide(app: &AppHandle) {
    state().showing.clear();
    if let Some(card) = app.get_webview_window(CARD_LABEL) {
        let _ = card.hide();
    }
    set_badge(app, false);
}

/// Clicking the card made CQ the active app. With no window of ours left on
/// screen, hand focus back to whatever the user was working in.
fn return_focus(app: &AppHandle) {
    let main_visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if main_visible {
        return;
    }
    let _ = app.run_on_main_thread(|| {
        // Safe: `run_on_main_thread` guarantees exactly that.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        NSApplication::sharedApplication(mtm).deactivate();
    });
}

fn play(app: &AppHandle, name: String) {
    let _ = app.run_on_main_thread(move || {
        if let Some(sound) = NSSound::soundNamed(&NSString::from_str(&name)) {
            // Start over if it's still ringing from the last time.
            sound.stop();
            sound.play();
        }
    });
}

/// The orange dot on the menu-bar icon.
///
/// The icon is a template image, which macOS draws in a single colour, so the
/// dot can't be part of it: it's a small view laid over the status item's
/// button. `with_inner_tray_icon` blocks on the main thread, so — like
/// `refresh_tray` — this always goes through a spawned thread.
fn set_badge(app: &AppHandle, on: bool) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
        let _ = tray.with_inner_tray_icon(move |inner| {
            // Runs on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let Some(button) = inner.ns_status_item().and_then(|item| item.button(mtm)) else {
                return;
            };
            for view in button.subviews().iter() {
                if view.isKindOfClass(NSBox::class()) {
                    view.removeFromSuperview();
                }
            }
            if !on {
                return;
            }
            const SIZE: f64 = 7.0;
            let bounds = button.bounds();
            let x = bounds.size.width - SIZE - 1.0;
            let y = if button.isFlipped() { 2.0 } else { bounds.size.height - SIZE - 2.0 };
            let dot = NSBox::initWithFrame(
                NSBox::alloc(mtm),
                NSRect::new(NSPoint::new(x, y), NSSize::new(SIZE, SIZE)),
            );
            dot.setBoxType(NSBoxType::Custom);
            dot.setTitlePosition(NSTitlePosition::NoTitle);
            dot.setBorderWidth(0.0);
            dot.setCornerRadius(SIZE / 2.0);
            // Jotter's orange, #d1552b.
            dot.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                209.0 / 255.0,
                85.0 / 255.0,
                43.0 / 255.0,
                1.0,
            ));
            button.addSubview(&dot);
        });
    });
}

/// Top-right of the usable area of the screen the pointer is on, in physical
/// pixels — the same space the monitor's own geometry is reported in.
fn corner(app: &AppHandle) -> Option<(i32, i32)> {
    let pointer = app.cursor_position().ok()?;
    let screen = app
        .monitor_from_point(pointer.x, pointer.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = screen.scale_factor();
    let area = screen.work_area();
    let x = area.position.x + area.size.width as i32 - ((CARD_WIDTH + CARD_MARGIN) * scale).round() as i32;
    let y = area.position.y + (CARD_MARGIN * scale).round() as i32;
    Some((x, y))
}

// ---- Commands ----------------------------------------------------------------------

/// The card's window has drawn a card: size it, place it, show it.
#[tauri::command]
pub fn reminder_present(app: AppHandle, height: f64) {
    let Some(card) = app.get_webview_window(CARD_LABEL) else { return };
    let _ = card.set_size(tauri::LogicalSize::new(CARD_WIDTH, height.clamp(80.0, 640.0)));
    if let Some((x, y)) = corner(&app) {
        let _ = card.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let _ = card.show();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(card) = handle.get_webview_window(CARD_LABEL) {
            crate::make_popup_float(&card);
            // Ordered in without activating, like the cursor popup.
            crate::order_popup_front(&card);
        }
    });
}

#[tauri::command]
pub fn reminder_dismiss(app: AppHandle) {
    hide(&app);
    return_focus(&app);
}

#[tauri::command]
pub fn reminder_snooze(app: AppHandle) {
    let until = Local::now().naive_local() + chrono::Duration::minutes(SNOOZE_MINUTES);
    {
        let mut st = state();
        let State { snoozed, showing, .. } = &mut *st;
        for id in showing.iter() {
            snoozed.insert(*id, until);
        }
    }
    hide(&app);
    return_focus(&app);
}

/// "Open Jotter": the control panel, on that folder's note.
#[tauri::command]
pub fn reminder_open(app: AppHandle, folder: u64) {
    hide(&app);
    crate::show_main(&app);
    let _ = app.emit_to("main", "jotter-open", folder);
}

/// "Show a test reminder": the card for one folder, now, whether or not its
/// reminders are on or anything in it is open.
#[tauri::command]
pub fn reminder_test(app: AppHandle, folder: u64) {
    show(&app, &[folder], true);
}

#[tauri::command]
pub fn reminder_preview_sound(app: AppHandle, name: String) {
    play(&app, name);
}

/// When `folder` next reminds, in milliseconds since the epoch for a JS `Date`.
#[tauri::command]
pub fn reminder_next(folder: u64) -> Option<i64> {
    let now = Local::now().naive_local();
    let st = state();
    let f = st.doc.folders.iter().find(|f| f.id == folder)?;
    if !f.reminder.on {
        return None;
    }
    let next = match st.snoozed.get(&folder) {
        Some(until) if *until > now => *until,
        _ => next_slot(&f.reminder, now)?,
    };
    Local.from_local_datetime(&next).earliest().map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, s).unwrap()
    }

    /// 14 September 2026 is a Monday.
    fn monday(h: u32, mi: u32, s: u32) -> NaiveDateTime {
        at(2026, 9, 14, h, mi, s)
    }

    fn working(every: u32) -> Settings {
        Settings { on: true, every, ..Settings::default() }
    }

    fn line(text: &str, depth: u32, done: bool) -> Line {
        Line { text: text.into(), depth, done }
    }

    fn state_with(reminder: Settings, lines: Vec<Line>) -> State {
        State {
            doc: Doc { folders: vec![Folder { id: 1, name: "Main Jots".into(), lines, reminder }] },
            ..State::default()
        }
    }

    #[test]
    fn times_line_up_with_the_clock() {
        let s = working(30);
        assert_eq!(latest_slot(&s, monday(10, 44, 10)), Some(monday(10, 30, 0)));
        assert_eq!(latest_slot(&s, monday(10, 0, 0)), Some(monday(10, 0, 0)));
        assert_eq!(latest_slot(&working(45), monday(9, 50, 0)), Some(monday(9, 45, 0)));
    }

    #[test]
    fn the_window_includes_both_ends_and_only_its_days() {
        let s = working(60);
        assert!(in_window(&s, monday(9, 0, 0)) && in_window(&s, monday(17, 0, 0)));
        assert!(!in_window(&s, monday(8, 0, 0)) && !in_window(&s, monday(18, 0, 0)));
        assert!(!in_window(&s, at(2026, 9, 19, 10, 0, 0)), "Saturday");
    }

    #[test]
    fn an_overnight_window_belongs_to_the_day_it_opened() {
        let fridays = Settings { on: true, every: 60, start: 22 * 60, end: 2 * 60, days: vec![5], sound: String::new() };
        assert!(in_window(&fridays, at(2026, 9, 18, 23, 0, 0)), "Friday 11pm");
        assert!(in_window(&fridays, at(2026, 9, 19, 1, 0, 0)), "early Saturday");
        assert!(!in_window(&fridays, at(2026, 9, 19, 23, 0, 0)), "Saturday 11pm");
    }

    #[test]
    fn each_time_fires_once() {
        let mut st = state_with(working(30), vec![line("call Sam", 0, false)]);
        assert_eq!(due_folders(&mut st, monday(10, 30, 5)), vec![1]);
        assert!(due_folders(&mut st, monday(10, 30, 40)).is_empty());
        assert_eq!(due_folders(&mut st, monday(11, 0, 2)), vec![1]);
    }

    #[test]
    fn a_time_slept_through_is_skipped() {
        let mut st = state_with(working(30), vec![line("call Sam", 0, false)]);
        assert!(due_folders(&mut st, monday(10, 32, 0)).is_empty());
    }

    #[test]
    fn off_or_nothing_open_means_no_reminder() {
        let mut off = state_with(Settings::default(), vec![line("call Sam", 0, false)]);
        assert!(due_folders(&mut off, monday(10, 0, 0)).is_empty());
        let mut done = state_with(working(30), vec![line("A", 0, true), line("a1", 1, false), line("  ", 0, false)]);
        assert!(due_folders(&mut done, monday(10, 0, 0)).is_empty());
    }

    #[test]
    fn a_snooze_holds_the_schedule_then_fires() {
        let mut st = state_with(working(15), vec![line("x", 0, false)]);
        st.snoozed.insert(1, monday(10, 10, 0));
        assert!(due_folders(&mut st, monday(10, 0, 5)).is_empty(), "regular 10:00 waits");
        assert_eq!(due_folders(&mut st, monday(10, 10, 3)), vec![1], "snooze ends");
        assert!(due_folders(&mut st, monday(10, 10, 20)).is_empty());
        assert_eq!(due_folders(&mut st, monday(10, 15, 1)), vec![1], "back on the schedule");
    }

    #[test]
    fn crossed_out_groups_are_not_open() {
        let lines = vec![line("A", 0, true), line("a1", 1, false), line("B", 0, false), line("b1", 1, false)];
        let open: Vec<_> = open_lines(&lines).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(open, vec!["B", "b1"]);
    }

    #[test]
    fn next_skips_the_weekend() {
        let s = working(60);
        assert_eq!(next_slot(&s, monday(10, 0, 0)), Some(monday(11, 0, 0)));
        assert_eq!(next_slot(&s, at(2026, 9, 18, 17, 30, 0)), Some(at(2026, 9, 21, 9, 0, 0)));
        let never = Settings { days: vec![], ..working(60) };
        assert_eq!(next_slot(&never, monday(10, 0, 0)), None);
    }

    #[test]
    fn reads_a_document_saved_before_reminders_existed() {
        let doc: Doc = serde_json::from_str(
            r#"{"version":1,"view":"jotter","folders":[{"id":3,"name":"Main Jots","lines":[{"text":"hi","depth":0,"done":false}],"unknown":true}]}"#,
        )
        .unwrap();
        assert_eq!(doc.folders[0].reminder, Settings::default());
        assert!(!doc.folders[0].reminder.on);
    }

    #[test]
    fn the_card_lists_the_first_open_lines_and_counts_the_rest() {
        let lines = ["a", "b", "c", "d", "e"].iter().map(|t| line(t, 0, false)).collect();
        let st = state_with(working(30), lines);
        let card = build_card(&st.doc, &[1]);
        assert_eq!(card.folders[0].items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert_eq!(card.folders[0].more, 2);
    }
}
