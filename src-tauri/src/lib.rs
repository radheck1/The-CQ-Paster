//! cQ — an ultra-minimal multi-slot clipboard manager.
//!
//! Lives in the tray. A global keyboard hook (see [`hook`]) implements the
//! `Ctrl+<N>+C` / `Ctrl+<N>+V` chords over 9 clipboard slots. On Windows there
//! are two modes: Master (no UI) and Noob (a reference popup by the cursor).
//! macOS has no modes — the popup always shows — and its control panel also
//! hosts CQ Jotter, a notepad (see [`jotter`]) with per-folder reminders (see
//! [`reminders`]).

mod clipboard;
mod hook;
#[cfg(target_os = "macos")]
mod dictate;
#[cfg(target_os = "macos")]
mod jotter;
#[cfg(target_os = "macos")]
mod reminders;
#[cfg(target_os = "macos")]
mod shake;
#[cfg(target_os = "macos")]
mod shotter;
mod permissions;
mod slots;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use slots::{FolderDto, FolderStore, SlotDto, SlotStore};

/// Per-user directory holding the persisted state: `%APPDATA%\com.cqpaster.app`
/// on Windows, `~/Library/Application Support/com.cqpaster.app` on macOS.
///
/// This must never resolve to a relative path. `SlotStore::save` discards its
/// errors, so an unwritable directory loses every slot silently — and a bundled
/// `.app` runs with the working directory set to `/`, where a relative fallback
/// is guaranteed to fail. See `ensure_data_dir`, which reports that at startup.
#[cfg(windows)]
fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.cqpaster.app")
}

#[cfg(target_os = "macos")]
fn data_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Library")
        .join("Application Support")
        .join("com.cqpaster.app")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.cqpaster.app")
}

/// Create the data directory up front and surface a failure.
///
/// Persistence is otherwise entirely silent: `SlotStore::save` swallows both the
/// `create_dir_all` and the `write` error, so a bad path looks like a working
/// app that forgets everything on quit. One check at startup turns that into a
/// visible message instead of a bug report weeks later.
fn ensure_data_dir() {
    let dir = data_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[cQ] cannot create data directory {} — slots will not persist: {e}",
            dir.display()
        );
    }
}

/// Append a line to the diagnostics log beside the persisted state.
///
/// A bundled `.app` has nowhere for `eprintln!` to go — GUI stderr is not
/// captured by the unified log — so without this the app cannot report anything
/// about itself once installed, which is exactly when the permission problems
/// happen. Kept deliberately dumb: no dependencies, no buffering, safe to call
/// from any thread. Never call it from the tap callback (6.1).
pub fn diag(msg: &str) {
    use std::io::Write;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{secs}] {msg}");
    eprintln!("[cQ] {line}");
    let path = data_dir().join("diagnostics.log");
    roll_if_big(&path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The log keeps at most this much, twice: the live file and one previous.
///
/// Small on purpose. What this file is for is the last thing that happened
/// before something went wrong, and a reader who has to search megabytes for
/// it will not find it. The file had reached 30 MB before this existed.
const LOG_MAX: u64 = 2 * 1024 * 1024;

/// Move the log aside once it is too big, keeping one generation.
///
/// Checked on every line rather than on a timer: the check is a `metadata`
/// call, and a log that is only trimmed while the app happens to be running a
/// timer is a log that grows unbounded in the cases that matter.
fn roll_if_big(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return; // not there yet
    };
    if meta.len() < LOG_MAX {
        return;
    }
    let previous = path.with_extension("log.1");
    // Rename rather than truncate: anything holding the old file keeps
    // writing to something real, and the previous generation survives for
    // exactly as long as it takes to fill another.
    let _ = std::fs::rename(path, &previous);
}

/// Where folders (and their slots) are persisted, so they survive restarts.
fn folders_file() -> PathBuf {
    data_dir().join("folders.bin")
}

/// The pre-folders store. Read once, to migrate existing slots into "Main".
fn legacy_slots_file() -> PathBuf {
    data_dir().join("slots.bin")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Master,
    Noob,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Master => "master",
            Mode::Noob => "noob",
        }
    }
}

/// Shared application state. Held both in Tauri's managed state (for commands)
/// and by the hook worker thread.
pub struct AppState {
    /// All folders plus the active pointer. Hotkeys act on the active folder.
    pub folders: Mutex<FolderStore>,
    pub mode: Mutex<Mode>,
    /// Monotonic counter used to debounce the popup auto-hide timer.
    pub popup_gen: AtomicU64,
    /// Slots taken just before the last "clear all", tagged with the folder they
    /// came from so undo restores them to the right place even if the user has
    /// switched folders since.
    pub last_cleared: Mutex<Option<(u64, SlotStore)>>,
    /// On-disk location the folders persist to.
    store_file: PathBuf,
}

impl AppState {
    fn new() -> Self {
        let store_file = folders_file();
        Self {
            folders: Mutex::new(FolderStore::load(&store_file, &legacy_slots_file())),
            mode: Mutex::new(Mode::Noob),
            popup_gen: AtomicU64::new(0),
            last_cleared: Mutex::new(None),
            store_file,
        }
    }

    pub fn to_dto(&self) -> StateDto {
        let mode = self.mode.lock().unwrap().as_str().to_string();
        let folders = self.folders.lock().unwrap();
        StateDto {
            mode,
            slots: folders.dtos(),
            folders: folders.folder_dtos(),
            active_folder: folders.active_id(),
            folder_name: folders.active_name(),
        }
    }

    /// Write the current folders to disk. Call after any change.
    pub fn persist(&self) {
        self.folders.lock().unwrap().save(&self.store_file);
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StateDto {
    pub mode: String,
    /// The active folder's 9 slots.
    pub slots: Vec<SlotDto>,
    pub folders: Vec<FolderDto>,
    pub active_folder: u64,
    pub folder_name: String,
}

// ---- Commands ----------------------------------------------------------------

#[tauri::command]
fn get_state(state: State<'_, Arc<AppState>>) -> StateDto {
    state.to_dto()
}

#[tauri::command]
fn set_mode(mode: String, state: State<'_, Arc<AppState>>) {
    let new = if mode == "master" {
        Mode::Master
    } else {
        Mode::Noob
    };
    *state.mode.lock().unwrap() = new;
    // No event emit: the frontend toggles the buttons in place so the colors
    // can cross-fade (a re-render would replace them and skip the transition).
}

/// Load a slot back onto the system clipboard so the user can paste it with a
/// normal Ctrl+V. Deliberately overwrites the current clipboard.
#[tauri::command]
fn copy_slot(index: usize, state: State<'_, Arc<AppState>>) -> bool {
    let snap = state.folders.lock().unwrap().get_snapshot(index);
    match snap {
        Some(s) => clipboard::restore(&s).is_ok(),
        None => false,
    }
}

/// The full text of a slot, for hovering to read a long item in the control
/// panel. Returns `None` for slots holding no text (images, file lists).
///
/// Capped: a slot can hold a multi-megabyte paste, and the point is to let a
/// user read what they copied, not to move the whole payload across the IPC
/// boundary and lay it out in one line box.
#[tauri::command]
fn slot_text(index: usize, state: State<'_, Arc<AppState>>) -> Option<String> {
    const MAX_CHARS: usize = 20_000;
    let snap = state.folders.lock().unwrap().get_snapshot(index)?;
    let text = clipboard::full_text(&snap)?;
    if text.chars().count() <= MAX_CHARS {
        return Some(text);
    }
    let mut out: String = text.chars().take(MAX_CHARS).collect();
    out.push('\u{2026}');
    Some(out)
}

/// A small PNG thumbnail of an image slot, as a `data:` URL.
///
/// Generated on demand and never persisted. The stored snapshot holds the image
/// at full size — a screenshot is comfortably over 100KB — and `SlotPreview` is
/// written into `folders.bin` for every slot of every folder, so putting a
/// thumbnail there would bloat the file and change its bincode layout.
///
/// Downscaling here rather than in CSS keeps the IPC payload to a few KB
/// instead of shipping a full screenshot across to be drawn 60px tall.
#[tauri::command]
fn slot_thumbnail(index: usize, state: State<'_, Arc<AppState>>) -> Option<String> {
    use base64::Engine as _;

    // Matches the CSS bounds. `thumbnail` fits within them and keeps the aspect
    // ratio, so a wide screenshot stays wide.
    const MAX_W: u32 = 120;
    const MAX_H: u32 = 60;

    let snap = state.folders.lock().unwrap().get_snapshot(index)?;
    let raw = clipboard::image_bytes(&snap)?;
    let decoded = image::load_from_memory(&raw).ok()?;
    let thumb = decoded.thumbnail(MAX_W, MAX_H);

    let mut png = std::io::Cursor::new(Vec::new());
    thumb.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    ))
}

#[tauri::command]
fn clear_slot(index: usize, app: AppHandle, state: State<'_, Arc<AppState>>) {
    state.folders.lock().unwrap().clear(index);
    sync(&app, state.inner());
}

/// Clear the **active** folder only. Other folders are untouched.
#[tauri::command]
fn clear_all(app: AppHandle, state: State<'_, Arc<AppState>>) {
    {
        let mut folders = state.folders.lock().unwrap();
        // Stash the pre-clear slots, tagged with their folder, so undo can put
        // them back even if the user switches folders in the meantime.
        *state.last_cleared.lock().unwrap() =
            Some((folders.active_id(), folders.active_slots_clone()));
        folders.clear_all();
    }
    sync(&app, state.inner());
}

/// Restore the slots taken before the last "clear all", into the folder they
/// were cleared from. A no-op if that folder has since been deleted.
#[tauri::command]
fn undo_clear(app: AppHandle, state: State<'_, Arc<AppState>>) -> bool {
    let restored = {
        let mut buf = state.last_cleared.lock().unwrap();
        match buf.take() {
            Some((id, prev)) => state.folders.lock().unwrap().replace_slots(id, prev),
            None => false,
        }
    };
    if restored {
        sync(&app, state.inner());
    }
    restored
}

// ---- Folder commands ---------------------------------------------------------

/// Create a folder and switch to it. Returns its id.
#[tauri::command]
fn create_folder(name: String, app: AppHandle, state: State<'_, Arc<AppState>>) -> u64 {
    let id = state.folders.lock().unwrap().create(&name);
    sync(&app, state.inner());
    id
}

#[tauri::command]
fn rename_folder(id: u64, name: String, app: AppHandle, state: State<'_, Arc<AppState>>) -> bool {
    let ok = state.folders.lock().unwrap().rename(id, &name);
    if ok {
        sync(&app, state.inner());
    }
    ok
}

/// Delete a folder and everything in it. Refuses to remove the last one.
#[tauri::command]
fn delete_folder(id: u64, app: AppHandle, state: State<'_, Arc<AppState>>) -> bool {
    let ok = state.folders.lock().unwrap().delete(id);
    if ok {
        // A pending undo aimed at this folder can never land now.
        let mut buf = state.last_cleared.lock().unwrap();
        if matches!(*buf, Some((cleared, _)) if cleared == id) {
            *buf = None;
        }
        drop(buf);
        sync(&app, state.inner());
    }
    ok
}

#[tauri::command]
fn select_folder(id: u64, app: AppHandle, state: State<'_, Arc<AppState>>) -> bool {
    let ok = state.folders.lock().unwrap().select(id);
    if ok {
        sync(&app, state.inner());
    }
    ok
}

#[tauri::command]
fn show_main_window(app: AppHandle) {
    show_main(&app);
}

// ---- Helpers -----------------------------------------------------------------

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Where to put a window of `size` so it hangs under `pointer` without leaving
/// `area`.
///
/// The pointer ends up at the **top centre** of the window, so it drops from
/// where the shake happened the way a menu drops from a click. Centred on the
/// pointer instead, half of it sits above, covering whatever was being pointed
/// at.
///
/// AppKit coordinates: y counts up from the bottom, so the top edge is
/// `origin.y + height`, and hanging below the pointer means `origin.y =
/// pointer.y - height`. `area` is the visible frame, so the menu bar and Dock
/// are already excluded.
///
/// Pure, so the clamping can be tested without a screen. Getting it wrong puts
/// the window where it cannot be reached.
#[cfg(target_os = "macos")]
pub(crate) fn under_pointer(area: (f64, f64, f64, f64), size: (f64, f64), pointer: (f64, f64)) -> (f64, f64) {
    let (ax, ay, aw, ah) = area;
    let (w, h) = size;
    // A window larger than the screen cannot be fitted; putting its origin at
    // the corner at least keeps its top-left reachable.
    let x = if w >= aw { ax } else { (pointer.0 - w / 2.0).clamp(ax, ax + aw - w) };
    // Shaking near the bottom leaves no room below, and the clamp lifts it
    // back up — staying on screen matters more than staying under the pointer.
    let y = if h >= ah { ay } else { (pointer.1 - h).clamp(ay, ay + ah - h) };
    (x, y)
}

/// Show the control panel where the pointer is.
///
/// Used by the shake gesture, which means "open, here" — opening it wherever it
/// happened to be last is the thing that makes a shake feel like it did not
/// work.
#[cfg(target_os = "macos")]
pub(crate) fn show_main_at_pointer(app: &AppHandle) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen, NSWindow};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window("main") else { return };
        let Ok(ptr) = win.ns_window() else { return };
        if ptr.is_null() {
            return;
        }
        // Safe: `run_on_main_thread` guarantees exactly that.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
        let pointer = NSEvent::mouseLocation();

        let screens = NSScreen::screens(mtm);
        let area = screens
            .iter()
            .find(|s| {
                let f = s.frame();
                pointer.x >= f.origin.x
                    && pointer.x <= f.origin.x + f.size.width
                    && pointer.y >= f.origin.y
                    && pointer.y <= f.origin.y + f.size.height
            })
            .or_else(|| NSScreen::mainScreen(mtm))
            .map(|s| s.visibleFrame());
        let Some(area) = area else { return };

        let frame = ns.frame();
        let (x, y) = under_pointer(
            (area.origin.x, area.origin.y, area.size.width, area.size.height),
            (frame.size.width, frame.size.height),
            (pointer.x, pointer.y),
        );
        // Placed before it is shown, so it never appears where it was last and
        // then jumps.
        ns.setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(frame.size.width, frame.size.height)), false);
        diag(&format!(
            "shake: panel at {x:.0},{y:.0} for a pointer at {:.0},{:.0}",
            pointer.x, pointer.y
        ));
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    });
}

/// Persist, push the new state to the frontend, and re-skin the tray. Call
/// after any change to folders or slots.
fn sync(app: &AppHandle, state: &Arc<AppState>) {
    state.persist();
    let _ = app.emit("state-updated", state.to_dto());
    refresh_tray(app, state);
}

/// Rebuild the tray menu (the folder list is dynamic) and its tooltip.
///
/// IMPORTANT: this always runs on a spawned thread. Tauri's menu setters post a
/// task to the main-thread event loop and then block waiting for it — so
/// calling them *from* the main thread deadlocks, and menu-event handlers run
/// on the main thread.
pub(crate) fn refresh_tray(app: &AppHandle, state: &Arc<AppState>) {
    let app = app.clone();
    let state = state.clone();
    std::thread::spawn(move || {
        let Some(tray) = app.tray_by_id("cq-tray") else {
            return;
        };
        if let Ok(menu) = tray_menu(&app, &state) {
            let _ = tray.set_menu(Some(menu));
        }
        let name = state.folders.lock().unwrap().active_name();
        let _ = tray.set_tooltip(Some(format!("cQ — {name}")));
    });
}

// Windows only: macOS has no modes.
#[cfg(not(target_os = "macos"))]
fn toggle_mode(app: &AppHandle, state: &Arc<AppState>) {
    {
        let mut m = state.mode.lock().unwrap();
        *m = match *m {
            Mode::Master => Mode::Noob,
            Mode::Noob => Mode::Master,
        };
    }
    let _ = app.emit("state-updated", state.to_dto());
}

/// Build the tray menu. Rebuilt on every folder change, so the folder list and
/// the active-folder labels stay current.
/// Dictation's tray entry: a single item before the model is downloaded, and a
/// submenu with the microphone list afterwards. An enum because the two are
/// different types and both have to outlive the menu that borrows them.
#[cfg(target_os = "macos")]
enum DictateMenu {
    Plain(tauri::menu::MenuItem<tauri::Wry>),
    Sub(tauri::menu::Submenu<tauri::Wry>),
}

fn tray_menu(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{
        CheckMenuItemBuilder, IsMenuItem, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, Submenu,
    };
    use tauri_plugin_autostart::ManagerExt;

    let (folders, active_name) = {
        let f = state.folders.lock().unwrap();
        (f.folder_dtos(), f.active_name())
    };

    let open_i = MenuItemBuilder::with_id("open", "Open control panel").build(app)?;

    // Folder switcher. The submenu's own label carries the active folder, so the
    // answer to "which folder am I in?" is visible without opening it.
    let folder_items = folders
        .iter()
        .map(|f| {
            CheckMenuItemBuilder::with_id(
                format!("folder:{}", f.id),
                format!("{}  ({}/9)", f.name, f.filled),
            )
            .checked(f.active)
            .build(app)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let folder_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = folder_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<tauri::Wry>)
        .collect();
    let folder_sub = Submenu::with_items(app, format!("Folder: {active_name}"), true, &folder_refs)?;

    #[cfg(not(target_os = "macos"))]
    let mode_i = MenuItemBuilder::with_id("mode", "Toggle Master / Noob mode").build(app)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart_i = CheckMenuItemBuilder::with_id("autostart", "Start on login")
        .checked(autostart_on)
        .build(app)?;
    // Scoped to the active folder, and says so.
    let clear_i =
        MenuItemBuilder::with_id("clear", format!("Clear slots in \"{active_name}\"")).build(app)?;
    let quit_i = MenuItemBuilder::with_id("quit", "Quit cQ").build(app)?;

    // Shake the mouse to open the control panel. macOS only: it reads the mouse
    // through an event tap of its own.
    #[cfg(target_os = "macos")]
    let shake_sub = {
        let now = shake::settings();
        let on_i = CheckMenuItemBuilder::with_id("shake:on", "Shake the mouse to open")
            .checked(now.on)
            .build(app)?;
        let levels = [
            ("shake:high", "Sensitivity: high", shake::Sensitivity::High),
            ("shake:medium", "Sensitivity: medium", shake::Sensitivity::Medium),
            ("shake:low", "Sensitivity: low", shake::Sensitivity::Low),
        ];
        let level_items = levels
            .iter()
            .map(|(id, label, level)| {
                CheckMenuItemBuilder::with_id(*id, *label)
                    .checked(now.sensitivity == *level)
                    .enabled(now.on)
                    .build(app)
            })
            .collect::<tauri::Result<Vec<_>>>()?;
        let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&on_i];
        items.extend(level_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>));
        Submenu::with_items(app, "Shake to open", true, &items)?
    };

    // Dictation needs a 547 MB model before it can do anything, so the way in
    // is a window that fetches it rather than a switch that would silently do
    // nothing. Once the model is there, the microphone can be switched from
    // here without opening anything — the device you want often changes at the
    // moment you are about to speak, not while you are in a settings window.
    #[cfg(target_os = "macos")]
    let dictate_item = {
        let open_i = MenuItemBuilder::with_id(
            "dictate",
            if dictate::ready() {
                "Dictation\u{2026}"
            } else {
                "Set up dictation\u{2026}"
            },
        )
        .build(app)?;
        if !dictate::ready() {
            // Nothing to configure yet: one plain item rather than a submenu
            // whose only useful entry leads back to the same window.
            DictateMenu::Plain(open_i)
        } else {
            let mics = dictate::mic_menu();
            // The device list is read when the menu is built, which is each
            // time the tray is refreshed, so a microphone plugged in a moment
            // ago is there.
            let default_i = CheckMenuItemBuilder::with_id("mic:", "Follow the system default")
                .checked(mics.chosen.is_none())
                .build(app)?;
            let device_items = mics
                .devices
                .iter()
                .map(|m| {
                    CheckMenuItemBuilder::with_id(format!("mic:{}", m.id), &m.label)
                        .checked(mics.chosen.as_deref() == Some(m.id.as_str()))
                        .build(app)
                })
                .collect::<tauri::Result<Vec<_>>>()?;
            let lock_i = CheckMenuItemBuilder::with_id("mic-lock", "Always use this microphone")
                .checked(mics.locked)
                // Locking to "whatever the system picks" means nothing.
                .enabled(mics.chosen.is_some())
                .build(app)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&open_i, &sep, &default_i];
            items.extend(device_items.iter().map(|i| i as &dyn IsMenuItem<tauri::Wry>));
            items.push(&lock_i);
            DictateMenu::Sub(Submenu::with_items(app, "Dictation", true, &items)?)
        }
    };
    #[cfg(target_os = "macos")]
    let dictate_i: &dyn IsMenuItem<tauri::Wry> = match &dictate_item {
        DictateMenu::Plain(i) => i,
        DictateMenu::Sub(s) => s,
    };

    MenuBuilder::new(app)
        .items(&[
            &open_i,
            &folder_sub,
            #[cfg(not(target_os = "macos"))]
            &mode_i,
            #[cfg(target_os = "macos")]
            &shake_sub,
            #[cfg(target_os = "macos")]
            dictate_i,
            &autostart_i,
            &clear_i,
            &quit_i,
        ])
        .build()
}

fn build_tray(app: &AppHandle, state: Arc<AppState>) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri_plugin_autostart::ManagerExt;

    let menu = tray_menu(app, &state)?;
    let menu_state = state.clone();
    let tooltip = format!("cQ — {}", state.folders.lock().unwrap().active_name());

    // Match the tray icon to the taskbar's light/dark theme.
    #[cfg(windows)]
    let tray_icon = tray_icon_image(system_uses_light_theme());
    #[cfg(target_os = "macos")]
    let tray_icon = macos_tray_icon();
    #[cfg(not(any(windows, target_os = "macos")))]
    let tray_icon = app.default_window_icon().unwrap().clone();

    let builder = TrayIconBuilder::with_id("cq-tray").icon(tray_icon);

    // Not a template: the menu-bar icon carries CQ's own colours, so macOS
    // must draw it as it is rather than tinting it to match the bar. That also
    // means it looks the same in light and dark, which is the point of a
    // coloured mark — but it gives up the inverted state macOS draws while the
    // menu is open, where a template icon flips and this one will not.
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(false);

    builder
        .tooltip(tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();
            // Folder switch: ids are "folder:<id>".
            if let Some(rest) = id.strip_prefix("folder:") {
                if let Ok(fid) = rest.parse::<u64>() {
                    if menu_state.folders.lock().unwrap().select(fid) {
                        sync(app, &menu_state);
                    }
                }
                return;
            }
            #[cfg(target_os = "macos")]
            if id == "dictate" {
                dictate::open_window(app);
                return;
            }
            #[cfg(target_os = "macos")]
            if let Some(device) = id.strip_prefix("mic:") {
                if let Err(e) = dictate::choose_mic(device) {
                    diag(&format!("dictate: could not choose a microphone: {e}"));
                }
                refresh_tray(app, &state);
                return;
            }
            #[cfg(target_os = "macos")]
            if id == "mic-lock" {
                if let Err(e) = dictate::toggle_mic_lock() {
                    diag(&format!("dictate: could not lock the microphone: {e}"));
                }
                refresh_tray(app, &state);
                return;
            }
            #[cfg(target_os = "macos")]
            if let Some(rest) = id.strip_prefix("shake:") {
                if rest == "on" {
                    shake::toggle();
                } else {
                    shake::set_sensitivity(rest);
                }
                refresh_tray(app, &menu_state); // re-reads the saved setting
                return;
            }
            match id {
                "open" => show_main(app),
                #[cfg(not(target_os = "macos"))]
                "mode" => toggle_mode(app, &menu_state),
                "autostart" => {
                    let mgr = app.autolaunch();
                    let now = mgr.is_enabled().unwrap_or(false);
                    let _ = if now { mgr.disable() } else { mgr.enable() };
                    // Rebuild rather than set_checked: the menu is regenerated
                    // on folder changes anyway, and it re-reads the real state.
                    refresh_tray(app, &menu_state);
                }
                "clear" => {
                    {
                        let mut folders = menu_state.folders.lock().unwrap();
                        *menu_state.last_cleared.lock().unwrap() =
                            Some((folders.active_id(), folders.active_slots_clone()));
                        folders.clear_all();
                    }
                    sync(app, &menu_state);
                }
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    // Set the control-panel window's title-bar icon to match the theme, and
    // re-skin both the tray and window icons live on light/dark changes.
    #[cfg(windows)]
    {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.set_icon(tray_icon_image(system_uses_light_theme()));
        }
        spawn_theme_watcher(app.clone());
    }

    Ok(())
}

/// The menu-bar icon.
///
/// CQ's own colours rather than a template. A template image is drawn from its
/// alpha alone and tinted by macOS, which handles light, dark and the inverted
/// state while the menu is open without being asked — everything a coloured
/// icon gives up. It is kept here (`tray-black.png`) in case the colour turns
/// out not to read at menu-bar size.
#[cfg(target_os = "macos")]
fn macos_tray_icon() -> tauri::image::Image<'static> {
    static COLOUR: &[u8] = include_bytes!("../icons/tray-colour.png");
    tauri::image::Image::from_bytes(COLOUR).expect("decode tray icon")
}

/// Read the taskbar (system) light/dark setting. True = light taskbar.
#[cfg(windows)]
fn system_uses_light_theme() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ret = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    // ERROR_SUCCESS (0) and value 1 = light taskbar; default to dark otherwise.
    ret == 0 && data == 1
}

/// The tray icon matching the taskbar theme: black CQ on a light taskbar,
/// white CQ on a dark one.
#[cfg(windows)]
fn tray_icon_image(light_taskbar: bool) -> tauri::image::Image<'static> {
    static WHITE: &[u8] = include_bytes!("../icons/tray-white.png");
    static BLACK: &[u8] = include_bytes!("../icons/tray-black.png");
    let bytes = if light_taskbar { BLACK } else { WHITE };
    tauri::image::Image::from_bytes(bytes).expect("decode tray icon")
}

/// Poll the system theme and re-skin the tray icon when it changes.
#[cfg(windows)]
fn spawn_theme_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = system_uses_light_theme();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let now = system_uses_light_theme();
            if now != last {
                last = now;
                if let Some(tray) = app.tray_by_id("cq-tray") {
                    let _ = tray.set_icon(Some(tray_icon_image(now)));
                }
                // Also re-skin the window (taskbar-button / alt-tab) icon.
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.set_icon(tray_icon_image(now));
                }
            }
        }
    });
}

/// Let the popup appear over full-screen apps and on every Space.
///
/// Unlike Windows, macOS needs nothing done about activation: the popup is
/// declared `"focus": false`, which already stops it becoming key, so there is
/// no counterpart to `make_non_activating` here — verified by pasting into a
/// focused text field with the popup up and watching the caret keep blinking.
///
/// What macOS *does* need is collection behaviour. "Always on top" only orders
/// the window within its own Space, so without `FullScreenAuxiliary` the popup
/// silently fails to draw over a full-screen app — exactly when the user is
/// most focused on one thing.
/// The level the popup sits at.
///
/// `NSStatusWindowLevel` (25) is not enough to clear another app's full-screen
/// Space, so this uses `NSPopUpMenuWindowLevel` — the level menus themselves
/// use, which is the behaviour wanted here: visible over anything, including a
/// full-screen window belonging to a different application.
#[cfg(target_os = "macos")]
const POPUP_WINDOW_LEVEL: isize = 101;

/// Apply the floating behaviour, and report what actually stuck.
///
/// Called on every show, not just at startup: Tauri applies its own
/// `alwaysOnTop` handling (which sets `NSFloatingWindowLevel`, well below what
/// is needed here) and re-asserting afterwards is cheaper than depending on the
/// ordering between the two.
#[cfg(target_os = "macos")]
pub(crate) fn make_popup_float(win: &tauri::WebviewWindow) -> Option<(isize, usize)> {
    use objc2_app_kit::{NSColor, NSWindow, NSWindowCollectionBehavior};

    let ptr = win.ns_window().ok()?;
    if ptr.is_null() {
        return None;
    }
    let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
    // `CanJoinAllSpaces` puts the popup on every Space including full-screen
    // ones; `FullScreenAuxiliary` lets it coexist with a full-screen window
    // rather than forcing a Space switch. `Stationary` is deliberately absent —
    // it pins a window to its Space, which is the opposite of what is wanted.
    ns.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns.setLevel(POPUP_WINDOW_LEVEL);

    // Make the window's own transparency explicit rather than relying on it
    // being applied at some point after the first show.
    //
    // The popup's CSS background is translucent (0.8 alpha), which only reads
    // as translucent over something transparent. While the window is still
    // opaque, that 0.8 composites over a solid surface and looks completely
    // solid — the "opaque for a beat, then frosted" flash on first appearance.
    // Setting it here, and again on every show, means there is no first frame
    // where it can be wrong.
    ns.setOpaque(false);
    ns.setBackgroundColor(Some(&NSColor::clearColor()));
    Some((ns.level(), ns.collectionBehavior().0 as usize))
}

/// Give the control panel a native macOS title bar.
///
/// The window is declared `decorations: false` in the shared config, which is
/// right for Windows — it draws its own title bar with its own buttons. macOS
/// instead gets the system traffic lights, floating over content that extends
/// up behind them, which is also what makes the window corners round without
/// any CSS involvement.
///
/// Done at runtime rather than in `tauri.conf.json` so the shipping Windows
/// build reads exactly the config it reads today.
///
/// Must run on the main thread; `setup` already does.
#[cfg(target_os = "macos")]
pub(crate) fn make_native_titlebar(app: &AppHandle, label: &'static str) {
    use objc2_app_kit::{
        NSColor, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
    };

    // The style mask is composed here rather than by asking Tauri for
    // decorations. `set_decorations(true)` does not apply in time to be built
    // on: reading the mask afterwards showed Titled and Closable still absent,
    // so `standardWindowButton` found no buttons to hide and the overlay style
    // was applied to a window that later grew its own separate title bar —
    // visible as a white strip above the dark one, with a live zoom button.
    //
    // Still deferred to a later turn of the event loop so it runs after the
    // window is fully on screen.
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = handle.get_webview_window(label) else {
            return;
        };
        let Ok(ptr) = win.ns_window() else { return };
        if ptr.is_null() {
            return;
        }
        let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };

        // Titled gives the traffic lights; FullSizeContentView lets the web
        // view run up behind them, so the dark bar is continuous to the top
        // edge instead of sitting below a separate strip. Maximisable is
        // deliberately absent — the window does not zoom.
        ns.setStyleMask(
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::FullSizeContentView,
        );
        ns.setTitlebarAppearsTransparent(true);
        ns.setTitleVisibility(NSWindowTitleVisibility::Hidden);

        // Paint the window itself the same charcoal as the title bar. With
        // FullSizeContentView the window's own background is exposed along the
        // top edge, and its default light `windowBackgroundColor` read as a
        // white hairline above the dark bar. Matching `--bar` in styles.css
        // (#2c2f36), which is fixed across both themes.
        let bar =
            NSColor::colorWithSRGBRed_green_blue_alpha(44.0 / 255.0, 47.0 / 255.0, 54.0 / 255.0, 1.0);
        ns.setBackgroundColor(Some(&bar));

        // A one-pixel hairline remains along the top edge in light mode. It is
        // drawn by the window frame, not by us — the window's own background is
        // the charcoal set above, and forcing it away needs a dark window
        // appearance, which the web view inherits and which would pin the whole
        // UI to the dark theme. Following the system light/dark setting, as the
        // Windows build does, is worth more than the hairline costs.

        // Zoom does nothing here (the window is not maximizable), so it is
        // hidden rather than left as a dead green button.
        if let Some(zoom) = ns.standardWindowButton(NSWindowButton::ZoomButton) {
            zoom.setHidden(true);
        }
    });
}

/// Order the popup in without activating the app.
///
/// `orderFront:` is a request from an application that expects to be active,
/// and ours never is — it runs as an `Accessory` with no Dock icon, so when the
/// current Space belongs to someone else's full-screen window the request can
/// simply be dropped. `orderFrontRegardless` is the documented way to say "show
/// this even though I am not the active app", which is exactly this popup's
/// situation every single time it appears.
///
/// This does not focus the window and does not activate the app, so it does not
/// undo the non-activating behaviour the paste depends on.
#[cfg(target_os = "macos")]
pub(crate) fn order_popup_front(win: &tauri::WebviewWindow) {
    use objc2_app_kit::NSWindow;

    let Ok(ptr) = win.ns_window() else { return };
    if ptr.is_null() {
        return;
    }
    let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
    ns.orderFrontRegardless();
}

/// On Windows, strip the popup of activation so showing it never steals focus
/// from the app the user is pasting into.
#[cfg(windows)]
fn make_non_activating(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = hwnd.0 as isize as *mut core::ffi::c_void;
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                ex | WS_EX_NOACTIVATE as isize | WS_EX_TOOLWINDOW as isize,
            );
        }
    }
}

// ---- Entry point -------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    ensure_data_dir();
    diag(&format!(
        "--- launch: exe={:?} debug_build={}",
        std::env::current_exe(),
        cfg!(debug_assertions)
    ));
    let app_state = Arc::new(AppState::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(app_state.clone())
        .invoke_handler(tauri::generate_handler![
            get_state,
            set_mode,
            copy_slot,
            slot_text,
            slot_thumbnail,
            clear_slot,
            clear_all,
            undo_clear,
            create_folder,
            rename_folder,
            delete_folder,
            select_folder,
            show_main_window,
            #[cfg(target_os = "macos")]
            dictate::dictate_open,
            #[cfg(target_os = "macos")]
            dictate::dictate_close,
            #[cfg(target_os = "macos")]
            dictate::dictate_try,
            #[cfg(target_os = "macos")]
            dictate::dictate_vocab,
            #[cfg(target_os = "macos")]
            dictate::dictate_set_vocab,
            #[cfg(target_os = "macos")]
            dictate::dictate_suggestions,
            #[cfg(target_os = "macos")]
            dictate::dictate_decide,
            #[cfg(target_os = "macos")]
            dictate::dictate_lists,
            #[cfg(target_os = "macos")]
            dictate::dictate_set_lists,
            #[cfg(target_os = "macos")]
            dictate::dictate_mics,
            #[cfg(target_os = "macos")]
            dictate::dictate_set_mic,
            #[cfg(target_os = "macos")]
            dictate::dictate_release,
            #[cfg(target_os = "macos")]
            dictate::dictate_models,
            #[cfg(target_os = "macos")]
            dictate::dictate_download,
            #[cfg(target_os = "macos")]
            dictate::dictate_cancel,
            #[cfg(target_os = "macos")]
            jotter::jotter_load,
            #[cfg(target_os = "macos")]
            jotter::jotter_save,
            #[cfg(target_os = "macos")]
            reminders::reminder_present,
            #[cfg(target_os = "macos")]
            reminders::reminder_dismiss,
            #[cfg(target_os = "macos")]
            reminders::reminder_snooze,
            #[cfg(target_os = "macos")]
            reminders::reminder_open,
            #[cfg(target_os = "macos")]
            reminders::reminder_test,
            #[cfg(target_os = "macos")]
            reminders::reminder_preview_sound,
            #[cfg(target_os = "macos")]
            reminders::reminder_next,
            #[cfg(target_os = "macos")]
            shotter::shots_list,
            #[cfg(target_os = "macos")]
            shotter::shot_thumbnail,
            #[cfg(target_os = "macos")]
            shotter::shot_copy,
            #[cfg(target_os = "macos")]
            shotter::shot_trash,
            #[cfg(target_os = "macos")]
            shotter::shots_trash_all,
            #[cfg(target_os = "macos")]
            shotter::shot_rename,
            #[cfg(target_os = "macos")]
            shotter::shot_untrash,
            #[cfg(target_os = "macos")]
            shotter::shot_markup,
            #[cfg(target_os = "macos")]
            shotter::markup_current,
            #[cfg(target_os = "macos")]
            shotter::shot_image,
            #[cfg(target_os = "macos")]
            shotter::shot_save,
            #[cfg(target_os = "macos")]
            shotter::markup_close,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Menu-bar app: no Dock icon and no application menu. Without this
            // Tauri registers as a regular foreground app, which is wrong for
            // something that lives in the menu bar — and it makes "closing the
            // control panel keeps the app alive" behave differently, since the
            // Dock icon would keep offering a way back into a window-less app.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Popup: hidden, non-activating, click-through.
            if let Some(popup) = app.get_webview_window("popup") {
                let _ = popup.hide();
                #[cfg(windows)]
                make_non_activating(&popup);
                #[cfg(target_os = "macos")]
                make_popup_float(&popup);
                let _ = popup.set_ignore_cursor_events(true);
            }

            // Jotter's reminder card and its schedule.
            #[cfg(target_os = "macos")]
            reminders::start(app.handle());

            // Shotter's watch on the screenshot folder.
            #[cfg(target_os = "macos")]
            shotter::start(app.handle());

            // Shake the mouse to open the control panel.
            #[cfg(target_os = "macos")]
            shake::start(app.handle());

            // Dictation's rewrite engine holds about 6 GB while it is loaded.
            // Give that back when it goes unused; the next dictation reloads it
            // while the trigger is still held.
            #[cfg(target_os = "macos")]
            dictate::rewrite::watch_idle();

            // Main window: closing hides it instead of quitting the app.
            if let Some(main) = app.get_webview_window("main") {

                let w = main.clone();
                main.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // Enable "start on login" once, on the first run after install, so
            // it's on by default. Later toggles from the tray are respected.
            // Release only — in dev this would register the throwaway dev binary
            // and create the shared marker that the real installer checks.
            if !cfg!(debug_assertions) {
                use tauri_plugin_autostart::ManagerExt;
                let marker = data_dir().join("autostart.init");
                if !marker.exists() {
                    let _ = handle.autolaunch().enable();
                    if let Some(dir) = marker.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(&marker, b"1");
                }
            }

            #[cfg(target_os = "macos")]
            make_native_titlebar(&handle, "main");

            build_tray(&handle, app_state.clone())?;
            hook::start(handle.clone(), app_state.clone());
            // Walks the user through Accessibility and Input Monitoring. Both
            // are needed for the hotkeys, and only the first has a system
            // prompt — without this the app is simply inert and silent.
            permissions::start(handle.clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running cQ");
}

#[cfg(all(test, target_os = "macos"))]
mod placing_the_panel {
    use super::under_pointer;
    const AREA: (f64, f64, f64, f64) = (0.0, 0.0, 1512.0, 945.0);
    const PANEL: (f64, f64) = (442.0, 535.0);

    #[test]
    fn it_hangs_below_the_pointer_when_there_is_room() {
        // Horizontally centred on the pointer with its top edge there, so the
        // window drops away from what was being pointed at rather than over it.
        let (x, y) = under_pointer(AREA, PANEL, (756.0, 600.0));
        assert_eq!(x, 756.0 - PANEL.0 / 2.0);
        assert_eq!(y + PANEL.1, 600.0, "the top edge belongs at the pointer");
    }

    #[test]
    fn shaking_low_on_the_screen_lifts_it_back_into_view() {
        // No room below at all, so it cannot hang under the pointer. Staying
        // on screen wins.
        let (_, y) = under_pointer(AREA, PANEL, (756.0, 40.0));
        assert_eq!(y, AREA.1);
    }

    #[test]
    fn it_never_leaves_the_screen() {
        for p in [(0.0, 0.0), (1512.0, 0.0), (0.0, 945.0), (1512.0, 945.0), (1512.0, 472.0)] {
            let (x, y) = under_pointer(AREA, PANEL, p);
            assert!(x >= AREA.0, "{p:?} put it off the left");
            assert!(y >= AREA.1, "{p:?} put it off the bottom");
            assert!(x + PANEL.0 <= AREA.0 + AREA.2, "{p:?} put it off the right");
            assert!(y + PANEL.1 <= AREA.1 + AREA.3, "{p:?} put it off the top");
        }
    }

    #[test]
    fn a_screen_that_is_not_at_the_origin_is_handled() {
        let area = (1512.0, -200.0, 1920.0, 1080.0);
        let (x, y) = under_pointer(area, PANEL, (1512.0, -200.0));
        assert!(x >= 1512.0 && y >= -200.0);
        let (x, y) = under_pointer(area, PANEL, (3432.0, 880.0));
        assert!(x + PANEL.0 <= 3432.0 && y + PANEL.1 <= 880.0);
    }

    #[test]
    fn a_window_taller_than_the_screen_still_has_a_reachable_corner() {
        let (x, y) = under_pointer(AREA, (2000.0, 2000.0), (700.0, 400.0));
        assert_eq!((x, y), (AREA.0, AREA.1));
    }
}

/// Does every macOS-only command in the handler list carry its own gate?
///
/// `generate_handler!` takes a `#[cfg]` per entry, and the attribute applies
/// to the **one** entry after it. Adding a command by inserting a line before
/// an existing one therefore steals that entry's gate and leaves two commands
/// ungated — which compiles perfectly on macOS and breaks the Windows build,
/// where the module does not exist. That is exactly how it broke, and nothing
/// on a Mac can notice it: `cargo check` here is happy either way.
///
/// So the source is read at compile time and checked. It is a crude test, and
/// it is the only kind that can catch this without a Windows machine.
#[cfg(test)]
mod windows_build {
    /// Modules that only exist on macOS. A command from one of these in the
    /// handler list must be gated.
    const MAC_ONLY: &[&str] = &["dictate::", "jotter::", "reminders::", "shake::", "shotter::"];
    const GATE: &str = "#[cfg(target_os = \"macos\")]";

    #[test]
    fn every_macos_only_command_is_gated() {
        let source = include_str!("lib.rs");
        let lines: Vec<&str> = source.lines().map(str::trim).collect();
        let mut ungated = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            // A registration line: "dictate::dictate_mics," and nothing else.
            if !line.ends_with(',') || line.contains(' ') {
                continue;
            }
            if !MAC_ONLY.iter().any(|m| line.starts_with(m)) {
                continue;
            }
            let gated = i > 0 && lines[i - 1] == GATE;
            if !gated {
                ungated.push(format!("line {}: {line}", i + 1));
            }
        }
        assert!(
            ungated.is_empty(),
            "these commands would not compile on Windows — each needs its own \
             {GATE} on the line above it:\n  {}",
            ungated.join("\n  ")
        );
    }

    #[test]
    fn the_check_would_actually_catch_it() {
        // Guards the guard: if the line-shape match above ever stops matching
        // a registration line, the test passes vacuously and protects nothing.
        let source = include_str!("lib.rs");
        let found = source
            .lines()
            .map(str::trim)
            .filter(|l| l.ends_with(',') && !l.contains(' ') && l.starts_with("dictate::"))
            .count();
        assert!(found > 10, "only matched {found} dictate commands — the shape test is wrong");
    }
}

