//! CQ Shotter (macOS only): the screenshots taken while CQ is running, newest
//! first.
//!
//! A thread looks in the folder macOS saves screenshots to once a second. Files
//! already there when it first looks are left alone; a file that turns up later
//! and carries macOS's screen-capture tag joins the front of the list. The list
//! is kept in `shotter.json`, so it survives a relaunch, and a screenshot whose
//! file has gone (deleted or moved in Finder) drops off it. Screenshots taken
//! while CQ is quit are never picked up.
//!
//! The commands here do what the control panel and the markup window ask of a
//! screenshot: a thumbnail, a copy onto the clipboard, a move to the Trash and
//! back, and saving a marked-up version over the original.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSEvent, NSScreen, NSWindow};
use objc2_foundation::{NSFileManager, NSPoint, NSRect, NSSize, NSString, NSURL};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::clipboard::{ClipItem, ClipSnapshot, ClipType};

/// How often the folder is looked at.
const SCAN: Duration = Duration::from_secs(1);
/// How often the Screenshot app's folder setting is read again.
const FOLDER_EVERY: Duration = Duration::from_secs(5);
/// How long a new file without the screen-capture tag keeps being checked, in
/// case the tag lands a moment after the file does.
const TAG_GRACE: Duration = Duration::from_secs(5);
const SCREEN_CAPTURE_TAG: &str = "com.apple.metadata:kMDItemIsScreenCapture";
/// Thumbnail bounds in pixels: twice the card's width and height cap in CSS, for
/// Retina.
const THUMB_W: u32 = 860;
const THUMB_H: u32 = 520;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub const MARKUP_LABEL: &str = "markup";
/// The markup window's title bar (38), toolbar (56) and the padding around the
/// image (12 a side), in points: the room it needs besides the image. Matches
/// `.mk-toolbar`, `.mk-stage` and `STAGE_PAD` in the frontend.
const MARKUP_CHROME: f64 = 38.0 + 56.0 + 24.0;
const MARKUP_SIDES: f64 = 24.0;
/// Wide enough for the whole toolbar.
const MARKUP_MIN: (f64, f64) = (660.0, 440.0);
/// The largest share of the screen the markup window takes.
const MARKUP_SHARE: f64 = 0.85;

// ---- The list ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    id: u64,
    path: PathBuf,
    /// When it was taken, in ms since the epoch.
    taken: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    shots: Vec<Entry>,
}

/// A screenshot as the control panel sees it.
#[derive(Clone, Serialize)]
pub struct ShotDto {
    id: u64,
    name: String,
    taken: i64,
    /// The file's modification time: a new value means a new thumbnail.
    version: i64,
    /// Only PNGs open in the markup window.
    editable: bool,
}

#[derive(Clone, Serialize)]
pub struct ShotList {
    /// The folder's name, for the empty state.
    folder: String,
    shots: Vec<ShotDto>,
}

struct Trashed {
    entry: Entry,
    in_trash: PathBuf,
}

#[derive(Default)]
struct State {
    saved: Saved,
    folder: PathBuf,
    /// Whether `seen` holds the folder's contents yet. False until the folder has
    /// been read once, which waits on the Desktop-access prompt.
    baselined: bool,
    /// Files in the folder that have been looked at.
    seen: HashSet<PathBuf>,
    /// New files without the tag yet, and when each was first seen.
    pending: HashMap<PathBuf, Instant>,
    /// What the last trash moved: one screenshot, or all of them. Undo puts it back.
    trashed: Vec<Trashed>,
    /// The screenshot the markup window is showing.
    markup: Option<u64>,
    /// Whether the last read of the folder worked, to log only the changes.
    readable: Option<bool>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn file() -> PathBuf {
    crate::data_dir().join("shotter.json")
}

fn is_hidden(path: &Path) -> bool {
    path.file_name().map_or(true, |n| n.as_bytes().starts_with(b"."))
}

fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

/// Whether macOS tagged the file as a screenshot.
fn has_tag(path: &Path) -> bool {
    let (Some(p), Ok(name)) = (c_path(path), CString::new(SCREEN_CAPTURE_TAG)) else {
        return false;
    };
    unsafe { libc::getxattr(p.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) >= 0 }
}

fn ms(time: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    time.ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

fn created_ms(path: &Path) -> i64 {
    let meta = std::fs::metadata(path).ok();
    meta.as_ref()
        .and_then(|m| ms(m.created()).or_else(|| ms(m.modified())))
        .unwrap_or_else(|| ms(Ok(std::time::SystemTime::now())).unwrap_or(0))
}

fn modified_ms(path: &Path) -> i64 {
    std::fs::metadata(path).ok().and_then(|m| ms(m.modified())).unwrap_or(0)
}

/// One look at the folder. Returns whether the list changed.
///
/// The first successful read only takes note of what is there: the list is for
/// screenshots taken from now on. After that, a new file is checked for the tag
/// until it has it or `TAG_GRACE` runs out.
fn scan(st: &mut State, folder: &Path, now: Instant) -> bool {
    let read = std::fs::read_dir(folder);
    let readable = read.is_ok();
    if st.readable != Some(readable) {
        st.readable = Some(readable);
        if !cfg!(test) {
            crate::diag(&format!(
                "shotter: {} {}",
                folder.display(),
                if readable { "is readable" } else { "can't be read (no Desktop access yet?)" }
            ));
        }
    }
    let Ok(read) = read else { return false };
    let files: HashSet<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !is_hidden(p) && p.is_file())
        .collect();

    if !st.baselined || st.folder != folder {
        st.folder = folder.to_path_buf();
        st.seen = files;
        st.pending.clear();
        st.baselined = true;
        return prune(st);
    }

    for path in &files {
        if st.seen.insert(path.clone()) {
            st.pending.insert(path.clone(), now);
        }
    }
    // A file that left the folder is forgotten, so one that comes back (say, put
    // back from the Trash in Finder) is looked at again.
    st.seen.retain(|p| files.contains(p));
    let listed: HashSet<PathBuf> = st.saved.shots.iter().map(|e| e.path.clone()).collect();
    let mut found = Vec::new();
    st.pending.retain(|path, since| {
        if !files.contains(path) || listed.contains(path) {
            return false;
        }
        if has_tag(path) {
            found.push(path.clone());
            return false;
        }
        now.duration_since(*since) < TAG_GRACE
    });
    // Oldest first, so the newest ends up at the front.
    found.sort_by_key(|p| created_ms(p));
    let added = !found.is_empty();
    for path in found {
        st.saved.next_id += 1;
        let entry = Entry { id: st.saved.next_id, taken: created_ms(&path), path };
        if !cfg!(test) {
            crate::diag(&format!("shotter: added {}", entry.path.display()));
        }
        st.saved.shots.insert(0, entry);
    }
    prune(st) || added
}

/// Drop screenshots whose file is gone.
fn prune(st: &mut State) -> bool {
    let before = st.saved.shots.len();
    st.saved.shots.retain(|e| e.path.is_file());
    st.saved.shots.len() != before
}

fn dto(entry: &Entry) -> ShotDto {
    let ext = entry.path.extension().map(|e| e.to_string_lossy().to_lowercase());
    ShotDto {
        id: entry.id,
        name: entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        taken: entry.taken,
        version: modified_ms(&entry.path),
        editable: ext.as_deref() == Some("png"),
    }
}

fn list(st: &State) -> ShotList {
    let folder = if st.folder.as_os_str().is_empty() { screenshot_folder() } else { st.folder.clone() };
    ShotList {
        folder: folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        shots: st.saved.shots.iter().map(dto).collect(),
    }
}

/// Save the list and send it to the control panel.
fn publish(app: &AppHandle) {
    let (json, shots) = {
        let st = state();
        (serde_json::to_string(&st.saved), list(&st))
    };
    match json {
        Ok(json) => {
            if let Err(e) = crate::jotter::save(&file(), &json) {
                crate::diag(&format!("shotter: could not save the list: {e}"));
            }
        }
        Err(e) => crate::diag(&format!("shotter: could not write the list: {e}")),
    }
    let _ = app.emit_to("main", "shots-changed", shots);
}

fn path_of(id: u64) -> Option<PathBuf> {
    state().saved.shots.iter().find(|e| e.id == id).map(|e| e.path.clone())
}

// ---- Where screenshots go ------------------------------------------------------------

/// The Screenshot app's "Save to" folder, with a leading `~` expanded, or the
/// Desktop when it isn't set.
fn resolve_folder(location: Option<&str>, home: &Path) -> PathBuf {
    match location.map(str::trim).filter(|l| !l.is_empty()) {
        Some("~") => home.to_path_buf(),
        Some(l) if l.starts_with("~/") => home.join(&l[2..]),
        Some(l) => PathBuf::from(l),
        None => home.join("Desktop"),
    }
}

fn screenshot_folder() -> PathBuf {
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFPreferencesAppSynchronize(app: *const c_void) -> u8;
        fn CFPreferencesCopyAppValue(key: *const c_void, app: *const c_void) -> *mut c_void;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let key = NSString::from_str("location");
    let domain = NSString::from_str("com.apple.screencapture");
    // Both strings are toll-free bridged to CFString. The copy is +1.
    let value = unsafe {
        CFPreferencesAppSynchronize(Retained::as_ptr(&domain).cast());
        let raw = CFPreferencesCopyAppValue(Retained::as_ptr(&key).cast(), Retained::as_ptr(&domain).cast());
        Retained::<objc2::runtime::AnyObject>::from_raw(raw.cast())
    };
    let location = value.and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string());
    let folder = resolve_folder(location.as_deref(), &home);
    if folder.is_dir() {
        folder
    } else {
        home.join("Desktop")
    }
}

// ---- Running it ----------------------------------------------------------------------

/// Read the saved list and start watching the screenshot folder.
pub fn start(app: &AppHandle) {
    if let Ok(text) = std::fs::read_to_string(file()) {
        match serde_json::from_str::<Saved>(&text) {
            Ok(saved) => state().saved = saved,
            Err(e) => crate::diag(&format!("shotter: could not read the saved list, starting empty: {e}")),
        }
    }
    prune(&mut state());

    let app = app.clone();
    std::thread::spawn(move || {
        let mut folder = screenshot_folder();
        let mut folder_read = Instant::now();
        loop {
            if folder_read.elapsed() >= FOLDER_EVERY {
                folder = screenshot_folder();
                folder_read = Instant::now();
            }
            let changed = scan(&mut state(), &folder, Instant::now());
            if changed {
                publish(&app);
            }
            std::thread::sleep(SCAN);
        }
    });
}

// ---- Commands: the control panel ------------------------------------------------------

#[tauri::command]
pub fn shots_list() -> ShotList {
    list(&state())
}

/// A thumbnail for the card, as a `data:` URL. Decoded off the main thread: a
/// full-screen Retina screenshot takes a noticeable moment.
#[tauri::command]
pub async fn shot_thumbnail(id: u64) -> Option<String> {
    let path = path_of(id)?;
    tauri::async_runtime::spawn_blocking(move || thumbnail(&path)).await.ok().flatten()
}

fn thumbnail(path: &Path) -> Option<String> {
    use base64::Engine as _;
    let thumb = image::open(path).ok()?.thumbnail(THUMB_W, THUMB_H);
    let mut png = std::io::Cursor::new(Vec::new());
    thumb.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    ))
}

/// Put a screenshot on the clipboard as an image.
#[tauri::command]
pub fn shot_copy(id: u64) -> bool {
    path_of(id).is_some_and(|p| copy_image(&p))
}

fn copy_image(path: &Path) -> bool {
    let uti = match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("png") => "public.png",
        Some("jpg" | "jpeg") => "public.jpeg",
        Some("tif" | "tiff") => "public.tiff",
        Some("heic") => "public.heic",
        Some("pdf") => "com.adobe.pdf",
        _ => return false,
    };
    let Ok(data) = std::fs::read(path) else { return false };
    let snap = ClipSnapshot {
        items: vec![ClipItem { types: vec![ClipType { uti: uti.into(), data }] }],
    };
    crate::clipboard::restore(&snap).is_ok()
}

/// Move a screenshot's file to the Trash. The frontend offers Undo for a while.
#[tauri::command]
pub fn shot_trash(app: AppHandle, id: u64) -> bool {
    trash_where(&app, |e| e.id == id)
}

/// Clear Shots: move every listed screenshot to the Trash, with the same Undo.
#[tauri::command]
pub fn shots_trash_all(app: AppHandle) -> bool {
    trash_where(&app, |_| true)
}

fn trash_where(app: &AppHandle, pick: impl Fn(&Entry) -> bool) -> bool {
    let picked: Vec<Entry> = state().saved.shots.iter().filter(|e| pick(e)).cloned().collect();
    // Not under the lock: the Trash can take a moment, and the scan thread needs it.
    let mut gone = Vec::new();
    let mut undoable = Vec::new();
    for entry in picked {
        match move_to_trash(&entry.path) {
            Ok(landed) => {
                gone.push(entry.id);
                match landed {
                    Some(in_trash) => undoable.push(Trashed { entry, in_trash }),
                    None => crate::diag(&format!("shotter: {} went to the Trash, but not where to", entry.path.display())),
                }
            }
            Err(e) => crate::diag(&format!("shotter: could not move {} to the Trash: {e}", entry.path.display())),
        }
    }
    if gone.is_empty() {
        return false;
    }
    {
        let mut st = state();
        st.saved.shots.retain(|e| !gone.contains(&e.id));
        st.trashed = undoable;
    }
    publish(app);
    true
}

/// Where the file landed in the Trash, if macOS says.
fn move_to_trash(path: &Path) -> Result<Option<PathBuf>, String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let mut landed: Option<Retained<NSURL>> = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
        .map_err(|e| format!("{e:?}"))?;
    Ok(landed.and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string())))
}

/// Undo: move what the last trash took back out of the Trash and into the list.
#[tauri::command]
pub fn shot_untrash(app: AppHandle) -> bool {
    let restored = {
        // Held across the renames, so the scan thread can't see a file come back
        // and list it a second time before its entry is restored.
        let mut st = state();
        let mut back = Vec::new();
        for t in std::mem::take(&mut st.trashed) {
            if t.entry.path.exists() || std::fs::rename(&t.in_trash, &t.entry.path).is_err() {
                crate::diag(&format!("shotter: could not put {} back", t.entry.path.display()));
            } else {
                back.push(t.entry);
            }
        }
        let restored = !back.is_empty();
        restore_into(&mut st.saved.shots, back);
        restored
    };
    if restored {
        publish(&app);
    }
    restored
}

/// Put entries back into the list, newest first, skipping any file already listed.
fn restore_into(shots: &mut Vec<Entry>, back: Vec<Entry>) {
    for entry in back {
        if !shots.iter().any(|e| e.path == entry.path) {
            shots.push(entry);
        }
    }
    shots.sort_by(|a, b| b.taken.cmp(&a.taken));
}

// ---- Commands: the markup window ------------------------------------------------------

/// Open a screenshot in the markup window, creating the window the first time.
#[tauri::command]
pub fn shot_markup(app: AppHandle, id: u64) -> bool {
    let Some(path) = path_of(id) else { return false };
    let Some(size) = std::fs::read(&path).ok().and_then(|b| png_points(&b)) else { return false };
    state().markup = Some(id);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || open_markup(&handle, id, size));
    true
}

/// The screenshot the markup window should show, for a window that has just loaded.
#[tauri::command]
pub fn markup_current() -> Option<u64> {
    state().markup
}

/// The full image, as raw bytes.
#[tauri::command]
pub fn shot_image(id: u64) -> Result<tauri::ipc::Response, String> {
    let path = path_of(id).ok_or("that screenshot is no longer listed")?;
    std::fs::read(&path)
        .map(tauri::ipc::Response::new)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Done: save the marked-up PNG over the original, copy it, and close the window.
/// The image arrives as the raw request body, with the screenshot's id in a header.
#[tauri::command]
pub fn shot_save(app: AppHandle, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("expected the image as bytes".into());
    };
    let id = request
        .headers()
        .get("shot-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("missing the screenshot's id")?;
    let path = path_of(id).ok_or("that screenshot is no longer listed")?;
    save_over(&path, png)?;
    crate::diag(&format!("shotter: saved markup over {}", path.display()));
    copy_image(&path);
    close_markup(&app);
    publish(&app);
    Ok(())
}

/// Cancel, or after Done.
#[tauri::command]
pub fn markup_close(app: AppHandle) {
    close_markup(&app);
}

fn close_markup(app: &AppHandle) {
    state().markup = None;
    if let Some(win) = app.get_webview_window(MARKUP_LABEL) {
        let _ = win.hide();
    }
}

fn open_markup(app: &AppHandle, id: u64, image: (f64, f64)) {
    let win = match app.get_webview_window(MARKUP_LABEL) {
        Some(win) => win,
        None => {
            let built = tauri::WebviewWindowBuilder::new(app, MARKUP_LABEL, tauri::WebviewUrl::App("index.html".into()))
                .title("Markup")
                .inner_size(900.0, 640.0)
                .min_inner_size(MARKUP_MIN.0, MARKUP_MIN.1)
                .decorations(false)
                .visible(false)
                .build();
            let win = match built {
                Ok(win) => win,
                Err(e) => {
                    crate::diag(&format!("shotter: could not create the markup window: {e}"));
                    return;
                }
            };
            // Closing it keeps it for next time.
            let handle = app.clone();
            win.on_window_event(move |ev| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                    api.prevent_close();
                    close_markup(&handle);
                }
            });
            crate::make_native_titlebar(app, MARKUP_LABEL);
            win
        }
    };
    place_markup(&win, image);
    let _ = app.emit_to(MARKUP_LABEL, "markup-open", id);
    let _ = win.show();
    let _ = win.set_focus();
}

/// Size and centre the markup window on the screen under the pointer, in AppKit
/// points (see `reminders::place_card` for why not Tauri's positions). Main thread.
fn place_markup(win: &tauri::WebviewWindow, image: (f64, f64)) {
    let Ok(ptr) = win.ns_window() else { return };
    if ptr.is_null() {
        return;
    }
    // Safe: only called from `run_on_main_thread`.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let ns: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
    let screens = NSScreen::screens(mtm);
    let pointer = NSEvent::mouseLocation();
    let screen = screens
        .iter()
        .find(|s| {
            let f = s.frame();
            pointer.x >= f.origin.x
                && pointer.x <= f.origin.x + f.size.width
                && pointer.y >= f.origin.y
                && pointer.y <= f.origin.y + f.size.height
        })
        .or_else(|| NSScreen::mainScreen(mtm));
    let Some(screen) = screen else { return };
    ns.setFrame_display(markup_frame(screen.visibleFrame(), image), true);
}

/// The markup window's frame on a screen's visible area: the image at its own
/// size if that fits in `MARKUP_SHARE` of the screen, scaled down if not, never
/// smaller than `MARKUP_MIN`, and centred.
fn markup_frame(area: NSRect, image: (f64, f64)) -> NSRect {
    let max_w = area.size.width * MARKUP_SHARE;
    let max_h = area.size.height * MARKUP_SHARE;
    let (iw, ih) = (image.0.max(1.0), image.1.max(1.0));
    let scale = ((max_w - MARKUP_SIDES) / iw).min((max_h - MARKUP_CHROME) / ih).min(1.0);
    let w = (iw * scale + MARKUP_SIDES).max(MARKUP_MIN.0).min(max_w);
    let h = (ih * scale + MARKUP_CHROME).max(MARKUP_MIN.1).min(max_h);
    NSRect::new(
        NSPoint::new(
            area.origin.x + (area.size.width - w) / 2.0,
            area.origin.y + (area.size.height - h) / 2.0,
        ),
        NSSize::new(w, h),
    )
}

// ---- PNG and file details --------------------------------------------------------------

/// A PNG's chunks, each as its type and its whole bytes (length, type, data, CRC).
fn chunks(png: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    if !png.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let mut out = Vec::new();
    let mut at = PNG_SIGNATURE.len();
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().ok()?) as usize;
        let end = at.checked_add(12)?.checked_add(len)?;
        if end > png.len() {
            return None;
        }
        let kind: [u8; 4] = png[at + 4..at + 8].try_into().ok()?;
        out.push((kind, &png[at..end]));
        at = end;
        if &kind == b"IEND" {
            break;
        }
    }
    Some(out)
}

/// A PNG's size in points: pixels, halved for a 144 dpi Retina screenshot.
fn png_points(png: &[u8]) -> Option<(f64, f64)> {
    let chunks = chunks(png)?;
    let (_, ihdr) = chunks.iter().find(|(k, _)| k == b"IHDR")?;
    let w = u32::from_be_bytes(ihdr.get(8..12)?.try_into().ok()?) as f64;
    let h = u32::from_be_bytes(ihdr.get(12..16)?.try_into().ok()?) as f64;
    let scale = chunks
        .iter()
        .find(|(k, _)| k == b"pHYs")
        .and_then(|(_, c)| {
            let ppu = u32::from_be_bytes(c.get(8..12)?.try_into().ok()?) as f64;
            // Unit 1 is metres; 72 dpi is 2834.6 pixels per metre. Retina factors are
            // whole numbers, and the metric round trip isn't exact.
            (c.get(16) == Some(&1) && ppu > 0.0).then(|| (ppu * 0.0254 / 72.0).round().max(1.0))
        })
        .unwrap_or(1.0);
    Some((w / scale, h / scale))
}

/// `new` with the pixel density of `original`, so a Retina screenshot keeps
/// pasting at its real size once it has been marked up.
fn with_density_of(new: &[u8], original: &[u8]) -> Vec<u8> {
    let (Some(new_chunks), Some(old)) = (chunks(new), chunks(original)) else {
        return new.to_vec();
    };
    let Some(phys) = old.iter().find(|(k, _)| k == b"pHYs").map(|(_, c)| *c) else {
        return new.to_vec();
    };
    let mut out = PNG_SIGNATURE.to_vec();
    for (kind, chunk) in new_chunks {
        if &kind == b"pHYs" {
            continue;
        }
        out.extend_from_slice(chunk);
        if &kind == b"IHDR" {
            out.extend_from_slice(phys);
        }
    }
    out
}

/// Copy every extended attribute it can from one file to another: the
/// screen-capture tags (so Shotter and Spotlight still see a screenshot) and
/// Finder's own. Some, like `com.apple.macl`, can't be set, and are skipped.
fn copy_xattrs(from: &Path, to: &Path) {
    let (Some(f), Some(t)) = (c_path(from), c_path(to)) else { return };
    unsafe {
        let size = libc::listxattr(f.as_ptr(), std::ptr::null_mut(), 0, 0);
        if size <= 0 {
            return;
        }
        let mut names = vec![0u8; size as usize];
        let size = libc::listxattr(f.as_ptr(), names.as_mut_ptr().cast(), names.len(), 0);
        if size <= 0 {
            return;
        }
        for name in names[..size as usize].split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let Ok(name) = CString::new(name) else { continue };
            let len = libc::getxattr(f.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0);
            if len < 0 {
                continue;
            }
            let mut value = vec![0u8; len as usize];
            let len = libc::getxattr(f.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, 0);
            if len >= 0 {
                libc::setxattr(t.as_ptr(), name.as_ptr(), value.as_ptr().cast(), len as usize, 0, 0);
            }
        }
    }
}

/// Replace a screenshot with a new PNG, safely: written beside it, given its
/// attributes, density and permissions, flushed, and renamed over it.
fn save_over(path: &Path, png: &[u8]) -> Result<(), String> {
    if !png.starts_with(PNG_SIGNATURE) {
        return Err("the marked-up image is not a PNG".into());
    }
    let original = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let bytes = with_density_of(png, &original);
    let name = path.file_name().ok_or("the screenshot has no file name")?.to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.cq-markup"));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    copy_xattrs(path, &tmp);
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cq-shotter-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tag(path: &Path, name: &str, value: &[u8]) {
        let (p, n) = (c_path(path).unwrap(), CString::new(name).unwrap());
        let rc = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        assert_eq!(rc, 0, "setxattr {name}");
    }

    fn screenshot(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"png").unwrap();
        tag(&path, SCREEN_CAPTURE_TAG, b"bplist00\x09\x08");
        path
    }

    #[test]
    fn files_already_there_are_left_alone() {
        let dir = scratch("baseline");
        screenshot(&dir, "Screen Shot old.png");
        let mut st = State::default();
        let t0 = Instant::now();
        assert!(!scan(&mut st, &dir, t0));
        assert!(!scan(&mut st, &dir, t0 + SCAN));
        assert!(st.saved.shots.is_empty());
    }

    #[test]
    fn a_new_screenshot_joins_the_front_and_other_files_do_not() {
        let dir = scratch("new");
        let mut st = State::default();
        let t0 = Instant::now();
        scan(&mut st, &dir, t0);
        let first = screenshot(&dir, "Screen Shot 1.png");
        std::fs::write(dir.join("notes.txt"), b"not a screenshot").unwrap();
        std::fs::write(dir.join(".Screen Shot in progress.png"), b"hidden").unwrap();
        assert!(scan(&mut st, &dir, t0 + SCAN));
        std::thread::sleep(Duration::from_millis(20));
        let second = screenshot(&dir, "Screen Shot 2.png");
        assert!(scan(&mut st, &dir, t0 + SCAN * 2));
        let paths: Vec<_> = st.saved.shots.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![second, first]);
        assert_eq!(st.saved.shots[0].id, 2);
        assert!(!scan(&mut st, &dir, t0 + SCAN * 3), "nothing new");
    }

    #[test]
    fn a_tag_that_lands_late_still_counts_within_the_grace() {
        let dir = scratch("late");
        let mut st = State::default();
        let t0 = Instant::now();
        scan(&mut st, &dir, t0);
        let path = dir.join("Screen Shot late.png");
        std::fs::write(&path, b"png").unwrap();
        assert!(!scan(&mut st, &dir, t0 + SCAN));
        tag(&path, SCREEN_CAPTURE_TAG, b"1");
        assert!(scan(&mut st, &dir, t0 + SCAN * 2));
        assert_eq!(st.saved.shots.len(), 1);

        let untagged = dir.join("export.png");
        std::fs::write(&untagged, b"png").unwrap();
        scan(&mut st, &dir, t0 + SCAN * 3);
        scan(&mut st, &dir, t0 + SCAN * 3 + TAG_GRACE);
        assert!(!st.pending.contains_key(&untagged), "given up on after the grace");
        tag(&untagged, SCREEN_CAPTURE_TAG, b"1");
        assert!(!scan(&mut st, &dir, t0 + SCAN * 4 + TAG_GRACE));
    }

    #[test]
    fn undo_puts_screenshots_back_newest_first_without_doubles() {
        let entry = |id: u64, taken: i64| Entry { id, path: PathBuf::from(format!("/s/{id}.png")), taken };
        // One screenshot arrived after the rest were cleared.
        let mut shots = vec![entry(9, 900)];
        restore_into(&mut shots, vec![entry(3, 300), entry(7, 700), entry(5, 500)]);
        assert_eq!(shots.iter().map(|e| e.id).collect::<Vec<_>>(), vec![9, 7, 5, 3]);
        // A file that's already listed isn't listed twice.
        restore_into(&mut shots, vec![entry(7, 700)]);
        assert_eq!(shots.len(), 4);
    }

    #[test]
    fn a_deleted_file_drops_off_the_list() {
        let dir = scratch("gone");
        let mut st = State::default();
        let t0 = Instant::now();
        scan(&mut st, &dir, t0);
        let path = screenshot(&dir, "Screen Shot gone.png");
        scan(&mut st, &dir, t0 + SCAN);
        std::fs::remove_file(&path).unwrap();
        assert!(scan(&mut st, &dir, t0 + SCAN * 2));
        assert!(st.saved.shots.is_empty());
    }

    #[test]
    fn an_unreadable_saved_list_is_not_fatal() {
        assert!(serde_json::from_str::<Saved>("{ nope").is_err());
        let saved: Saved = serde_json::from_str(r#"{"shots":[{"id":4,"path":"/x.png","taken":1}]}"#).unwrap();
        assert_eq!(saved.next_id, 0);
        assert_eq!(saved.shots.len(), 1);
    }

    #[test]
    fn the_folder_setting_is_read_like_the_screenshot_app_writes_it() {
        let home = Path::new("/Users/me");
        assert_eq!(resolve_folder(None, home), home.join("Desktop"));
        assert_eq!(resolve_folder(Some("  "), home), home.join("Desktop"));
        assert_eq!(resolve_folder(Some("~/Pictures/Shots"), home), home.join("Pictures/Shots"));
        assert_eq!(resolve_folder(Some("/Volumes/Work/Shots"), home), PathBuf::from("/Volumes/Work/Shots"));
    }

    /// A tiny PNG built by hand: IHDR, an optional pHYs, IDAT and IEND. CRCs
    /// aren't checked by anything here.
    fn png(w: u32, h: u32, dpi: Option<u32>) -> Vec<u8> {
        png_with(w, h, dpi, &[1, 2, 3])
    }

    fn png_with(w: u32, h: u32, dpi: Option<u32>, idat: &[u8]) -> Vec<u8> {
        let chunk = |kind: &[u8; 4], data: &[u8]| {
            let mut c = (data.len() as u32).to_be_bytes().to_vec();
            c.extend_from_slice(kind);
            c.extend_from_slice(data);
            c.extend_from_slice(&[0, 0, 0, 0]);
            c
        };
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut out = PNG_SIGNATURE.to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        if let Some(dpi) = dpi {
            let ppm = (dpi as f64 / 0.0254).round() as u32;
            let mut phys = ppm.to_be_bytes().to_vec();
            phys.extend_from_slice(&ppm.to_be_bytes());
            phys.push(1);
            out.extend(chunk(b"pHYs", &phys));
        }
        out.extend(chunk(b"IDAT", idat));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn retina_screenshots_measure_in_points() {
        assert_eq!(png_points(&png(2287, 956, Some(72))), Some((2287.0, 956.0)));
        assert_eq!(png_points(&png(2000, 1000, Some(144))), Some((1000.0, 500.0)));
        assert_eq!(png_points(&png(640, 480, None)), Some((640.0, 480.0)));
        assert_eq!(png_points(b"not a png"), None);
    }

    #[test]
    fn saving_keeps_the_density_the_tags_and_replaces_the_pixels() {
        let dir = scratch("save");
        let path = dir.join("Screen Shot save.png");
        std::fs::write(&path, png(2000, 1000, Some(144))).unwrap();
        tag(&path, SCREEN_CAPTURE_TAG, b"yes");
        tag(&path, "com.apple.metadata:kMDItemScreenCaptureType", b"selection");

        let marked = png_with(2000, 1000, None, &[9, 9, 9, 9]);
        save_over(&path, &marked).unwrap();

        let saved = std::fs::read(&path).unwrap();
        let parts = chunks(&saved).unwrap();
        let kinds: Vec<_> = parts.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![*b"IHDR", *b"pHYs", *b"IDAT", *b"IEND"]);
        assert_eq!(&parts[2].1[8..12], &[9, 9, 9, 9], "the new pixels");
        assert_eq!(png_points(&saved), Some((1000.0, 500.0)));
        assert!(has_tag(&path), "still tagged as a screenshot");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left: {leftovers:?}");
        assert!(save_over(&path, b"not a png").is_err());
    }

    #[test]
    fn the_markup_window_fits_the_screen() {
        let area = NSRect::new(NSPoint::new(1512.0, 418.0), NSSize::new(2560.0, 1410.0));
        // Small: the image at its own size, centred.
        let f = markup_frame(area, (1000.0, 600.0));
        assert_eq!((f.size.width, f.size.height), (1000.0 + MARKUP_SIDES, 600.0 + MARKUP_CHROME));
        assert_eq!(f.origin.x, 1512.0 + (2560.0 - 1000.0 - MARKUP_SIDES) / 2.0);
        // Huge: scaled into 85% of the screen, keeping the image's shape.
        let f = markup_frame(area, (5120.0, 2880.0));
        assert!(f.size.width <= 2560.0 * MARKUP_SHARE + 0.01 && f.size.height <= 1410.0 * MARKUP_SHARE + 0.01);
        let image_w = f.size.width - MARKUP_SIDES;
        let image_h = f.size.height - MARKUP_CHROME;
        assert!((image_w / image_h - 5120.0 / 2880.0).abs() < 0.01);
        // Tiny: never smaller than the minimum.
        let f = markup_frame(area, (120.0, 40.0));
        assert_eq!((f.size.width, f.size.height), MARKUP_MIN);
    }
}
