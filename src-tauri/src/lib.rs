//! CQ Paster — an ultra-minimal multi-slot clipboard manager.
//!
//! Lives in the tray. A global keyboard hook (see [`hook`]) implements the
//! `Ctrl+<N>+C` / `Ctrl+<N>+V` chords over 9 clipboard slots. Two modes:
//! Master (no UI) and Noob (a reference popup by the cursor).

mod clipboard;
mod hook;
mod slots;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use slots::{FolderDto, FolderStore, SlotDto, SlotStore};

/// Per-user directory holding the persisted state.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.cqpaster.app")
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
fn refresh_tray(app: &AppHandle, state: &Arc<AppState>) {
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
        let _ = tray.set_tooltip(Some(format!("CQ Paster — {name}")));
    });
}

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
fn tray_menu(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{CheckMenuItemBuilder, IsMenuItem, MenuBuilder, MenuItemBuilder, Submenu};
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

    let mode_i = MenuItemBuilder::with_id("mode", "Toggle Master / Noob mode").build(app)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart_i = CheckMenuItemBuilder::with_id("autostart", "Start on login")
        .checked(autostart_on)
        .build(app)?;
    // Scoped to the active folder, and says so.
    let clear_i =
        MenuItemBuilder::with_id("clear", format!("Clear slots in \"{active_name}\"")).build(app)?;
    let quit_i = MenuItemBuilder::with_id("quit", "Quit CQ Paster").build(app)?;

    MenuBuilder::new(app)
        .items(&[
            &open_i,
            &folder_sub,
            &mode_i,
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
    let tooltip = format!("CQ Paster — {}", state.folders.lock().unwrap().active_name());

    // Match the tray icon to the taskbar's light/dark theme.
    #[cfg(windows)]
    let tray_icon = tray_icon_image(system_uses_light_theme());
    #[cfg(not(windows))]
    let tray_icon = app.default_window_icon().unwrap().clone();

    TrayIconBuilder::with_id("cq-tray")
        .icon(tray_icon)
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
            match id {
                "open" => show_main(app),
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
            clear_slot,
            clear_all,
            undo_clear,
            create_folder,
            rename_folder,
            delete_folder,
            select_folder,
            show_main_window
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Popup: hidden, non-activating, click-through.
            if let Some(popup) = app.get_webview_window("popup") {
                let _ = popup.hide();
                #[cfg(windows)]
                make_non_activating(&popup);
                let _ = popup.set_ignore_cursor_events(true);
            }

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

            build_tray(&handle, app_state.clone())?;
            hook::start(handle.clone(), app_state.clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running CQ Paster");
}
