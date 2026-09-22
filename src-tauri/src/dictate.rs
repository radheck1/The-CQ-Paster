//! Dictation (macOS only): the models it needs, and fetching them.
//!
//! Speech recognition runs on this machine, against model files far too large
//! to ship inside the app — 547 MB for speech alone. So CQ arrives without
//! them and fetches them the first time dictation is used, into
//! `models/` beside the rest of its state.
//!
//! The fetching is done by `/usr/bin/curl` rather than an HTTP client compiled
//! into CQ. curl is part of macOS, it already handles redirects, proxies,
//! flaky networks and resuming a half-finished file, and using it costs the
//! build nothing. Progress does not come from parsing curl's output: the
//! download writes to a `.part` file, and a thread here watches that file grow
//! against a size we already know. Nothing to parse, nothing to misparse.
//!
//! A finished download is checked against its SHA-256 before it is put into
//! place, and only then renamed over. A file that fails the check is deleted:
//! a truncated model is worse than no model, because it fails at the point of
//! use rather than at the point of download.

pub mod capture;
pub mod engine;
pub mod indicator;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

/// How often the progress thread looks at the growing `.part` file. Fast
/// enough that the bar moves smoothly, slow enough to be free.
const POLL: Duration = Duration::from_millis(250);

/// A model CQ downloads on first use.
pub struct ModelSpec {
    /// Stable key used by the frontend and in events.
    pub id: &'static str,
    /// What this model does, in the user's words.
    pub label: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    /// Hugging Face stores an LFS file's SHA-256 as its object id, which is
    /// where these came from and what the downloaded bytes are checked against.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// Speech recognition only, for now. The rewrite model joins this list when
/// that half is built, and everything here already handles more than one.
pub const MODELS: &[ModelSpec] = &[ModelSpec {
    id: "whisper",
    label: "Speech recognition",
    file: "ggml-large-v3-turbo-q5_0.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
    sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    bytes: 574_041_195,
}];

/// Where a model by id ended up, whether or not it has been downloaded.
pub fn model_path(id: &str) -> Option<PathBuf> {
    MODELS
        .iter()
        .find(|m| m.id == id)
        .map(|m| models_dir().join(m.file))
}

pub fn models_dir() -> PathBuf {
    crate::data_dir().join("models")
}

fn final_path(spec: &ModelSpec) -> PathBuf {
    models_dir().join(spec.file)
}

fn part_path(spec: &ModelSpec) -> PathBuf {
    models_dir().join(format!("{}.part", spec.file))
}

/// What the control panel needs to decide what to show.
#[derive(Clone, Serialize)]
pub struct ModelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub total: u64,
    /// The model is present and was verified when it landed.
    pub have: bool,
    /// Bytes of a half-finished download, so a resumed one doesn't restart the
    /// bar at zero.
    pub downloaded: u64,
}

/// A step of a download, sent to the control panel as `dictate-progress`.
#[derive(Clone, Serialize)]
struct Progress {
    id: &'static str,
    label: &'static str,
    downloaded: u64,
    total: u64,
    /// One of: downloading, verifying, done, failed, cancelled.
    state: &'static str,
    message: Option<String>,
}

/// What to do about a `.part` file that is already there.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Nothing usable on disk: start at the beginning.
    Fresh,
    /// A partial file: ask curl to carry on from where it stopped.
    Resume,
    /// Exactly the right number of bytes already: skip straight to the
    /// checksum, which is the only thing that can say whether they are right.
    Verify,
    /// More bytes than the model has. Something else wrote this, or the size
    /// changed upstream; resuming would append to rubbish, so throw it away.
    Restart,
}

/// Decide from the sizes alone, so the decision can be tested without a
/// network or a file.
pub fn resume_plan(part_len: u64, total: u64) -> Plan {
    match part_len {
        0 => Plan::Fresh,
        n if n < total => Plan::Resume,
        n if n == total => Plan::Verify,
        _ => Plan::Restart,
    }
}

/// The curl command line for one model. Pure, and returned as owned strings so
/// a test can read it without spawning anything.
///
/// `--fail-with-body` matters: without it curl writes an error page into the
/// file and exits 0, and the only thing that would notice is the checksum,
/// after the user waited for a 547 MB "download" that was really a 404.
///
/// `--continue-at -` is always passed, not only when resuming a part file left
/// by an earlier attempt. Measured against the real Hugging Face CDN: on a
/// file that does not exist yet it simply starts at zero, and with it in place
/// each of the `--retry` attempts carries on from where the last one stopped.
/// Without it, a retry truncates and starts again — which on a flaky
/// connection means downloading the first few megabytes over and over.
pub fn curl_args(url: &str, part: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--location".into(),
        "--fail-with-body".into(),
        "--silent".into(),
        "--show-error".into(),
        "--retry".into(),
        "5".into(),
        "--retry-delay".into(),
        "2".into(),
        "--retry-all-errors".into(),
    ];
    a.push("--continue-at".into());
    a.push("-".into());
    a.push("--output".into());
    a.push(part.to_string_lossy().into_owned());
    a.push(url.into());
    a
}

/// Whole-file SHA-256, streamed so a 4.7 GB model is never held in memory.
pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn len_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

pub fn status() -> Vec<ModelStatus> {
    MODELS
        .iter()
        .map(|spec| ModelStatus {
            id: spec.id,
            label: spec.label,
            total: spec.bytes,
            have: final_path(spec).exists(),
            downloaded: len_of(&part_path(spec)),
        })
        .collect()
}

/// Is every model present?
pub fn ready() -> bool {
    MODELS.iter().all(|s| final_path(s).exists())
}

/// Set while a download runs, so a second press of the button doesn't start a
/// second curl writing to the same file.
static BUSY: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
/// The running curl, so cancelling can end it rather than wait for 547 MB.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

fn emit(app: &AppHandle, p: Progress) {
    // Both surfaces: the setup window draws the bar, and the control panel
    // wants to know when dictation becomes usable.
    let _ = app.emit_to(WINDOW, "dictate-progress", p.clone());
    let _ = app.emit_to("main", "dictate-progress", p);
}

/// Fetch every missing model, in order, reporting as it goes. Returns when all
/// of them are present, or at the first failure.
fn fetch_all(app: &AppHandle) {
    for spec in MODELS {
        if CANCEL.load(Ordering::SeqCst) {
            break;
        }
        if final_path(spec).exists() {
            continue;
        }
        if let Err(e) = fetch(app, spec) {
            crate::diag(&format!("dictate: {} failed: {e}", spec.id));
            emit(
                app,
                Progress {
                    id: spec.id,
                    label: spec.label,
                    downloaded: len_of(&part_path(spec)),
                    total: spec.bytes,
                    state: if CANCEL.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    message: Some(e),
                },
            );
            return;
        }
    }
}

fn fetch(app: &AppHandle, spec: &ModelSpec) -> Result<(), String> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let part = part_path(spec);
    let mut plan = resume_plan(len_of(&part), spec.bytes);
    if plan == Plan::Restart {
        crate::diag(&format!(
            "dictate: {} part file is larger than the model — starting again",
            spec.id
        ));
        let _ = std::fs::remove_file(&part);
        plan = Plan::Fresh;
    }

    if plan != Plan::Verify {
        crate::diag(&format!("dictate: fetching {} ({plan:?})", spec.id));
        let args = curl_args(spec.url, &part);
        let child = Command::new("/usr/bin/curl")
            .args(&args)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        *CHILD.lock().unwrap() = Some(child);

        // Watch the file grow. The download itself is curl's job; this thread
        // only reports, and stops when this download ends.
        //
        // The flag is per-download, not the global `BUSY`: that one is cleared
        // only after every model is finished, so waiting on it here would be a
        // thread joining a thread that is waiting for the joiner to finish.
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let reporter = {
            let app = app.clone();
            let part = part.clone();
            let running = running.clone();
            let (id, label, total) = (spec.id, spec.label, spec.bytes);
            std::thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    emit(
                        &app,
                        Progress {
                            id,
                            label,
                            downloaded: len_of(&part).min(total),
                            total,
                            state: "downloading",
                            message: None,
                        },
                    );
                    std::thread::sleep(POLL);
                }
            })
        };

        let out = {
            let mut guard = CHILD.lock().unwrap();
            match guard.take() {
                Some(c) => c.wait_with_output(),
                None => Err(std::io::Error::other("download was cancelled")),
            }
        };
        running.store(false, Ordering::SeqCst);
        let _ = reporter.join();
        let out = out.map_err(|e| format!("curl did not finish: {e}"))?;

        if CANCEL.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if why.is_empty() {
                format!("curl exited {}", out.status)
            } else {
                why
            });
        }
    }

    emit(
        app,
        Progress {
            id: spec.id,
            label: spec.label,
            downloaded: spec.bytes,
            total: spec.bytes,
            state: "verifying",
            message: None,
        },
    );

    let got = sha256_of(&part).map_err(|e| format!("cannot read the download: {e}"))?;
    if got != spec.sha256 {
        // A wrong file here fails later, at the moment of use, where it looks
        // like a broken feature rather than a broken download.
        let _ = std::fs::remove_file(&part);
        return Err("the download did not match its checksum, so it was discarded".into());
    }

    std::fs::rename(&part, final_path(spec))
        .map_err(|e| format!("cannot put the model into place: {e}"))?;
    crate::diag(&format!("dictate: {} is ready", spec.id));
    // The tray says "Set up dictation…" until there is something to run, so it
    // has to be rebuilt the moment that stops being true.
    if ready() {
        if let Some(state) = app.try_state::<std::sync::Arc<crate::AppState>>() {
            crate::refresh_tray(app, &state);
        }
    }
    emit(
        app,
        Progress {
            id: spec.id,
            label: spec.label,
            downloaded: spec.bytes,
            total: spec.bytes,
            state: "done",
            message: None,
        },
    );
    Ok(())
}

/// The setup window's label, matching `capabilities/dictate.json`.
pub const WINDOW: &str = "dictate";

/// Open the setup window, building it the first time. Closing it keeps it for
/// next time, the way the markup window does.
pub fn open_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(WINDOW) {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let built =
        tauri::WebviewWindowBuilder::new(app, WINDOW, tauri::WebviewUrl::App("index.html".into()))
            .title("Dictation")
            .inner_size(480.0, 340.0)
            .min_inner_size(420.0, 300.0)
            .decorations(false)
            .visible(false)
            .build();
    let win = match built {
        Ok(win) => win,
        Err(e) => {
            crate::diag(&format!("dictate: could not create the setup window: {e}"));
            return;
        }
    };
    // Closing keeps the window for next time, the way the markup window does,
    // and means a download started here is not interrupted by dismissing it.
    let handle = app.clone();
    win.on_window_event(move |ev| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
            api.prevent_close();
            if let Some(w) = handle.get_webview_window(WINDOW) {
                let _ = w.hide();
            }
        }
    });
    crate::make_native_titlebar(app, WINDOW);
    let _ = win.show();
    let _ = win.set_focus();
    crate::diag("dictate: setup window open");
}

/// Dismiss the setup window.
///
/// The frontend cannot close its own window: `core:window:default` grants 28
/// permissions and `allow-close` is not among them, so calling `close()` from
/// the web view is refused by the ACL and nothing happens. Every other window
/// here goes through a command for the same reason.
#[tauri::command]
pub fn dictate_close(app: AppHandle) {
    if let Some(win) = app.get_webview_window(WINDOW) {
        let _ = win.hide();
    }
}

#[tauri::command]
pub fn dictate_open(app: AppHandle) {
    open_window(&app);
}

/// Where a recording is written before it is handed to the engine. One file,
/// reused: a dictation is transcribed and done with, and keeping the audio
/// around would be a recording of the user that nobody asked for.
pub fn scratch_wav() -> PathBuf {
    crate::data_dir().join("dictation.wav")
}

/// What the end-to-end check heard, sent as an event rather than returned.
#[derive(Clone, Serialize)]
struct Heard {
    text: Option<String>,
    error: Option<String>,
}

/// Record for a few seconds and transcribe it, so the microphone, the WAV and
/// the engine can be checked end to end before the trigger exists.
///
/// The work happens on a thread of its own and the result arrives as an event.
/// A synchronous Tauri command runs on the main thread, and this one sleeps for
/// the length of the recording: done inline it beachballs the whole app, and —
/// worse — the listening mark never appears, because showing it queues
/// `orderFrontRegardless` onto the very thread the sleep is holding. The mark
/// only got its turn after the recording had already been put away.
#[tauri::command]
pub fn dictate_try(app: AppHandle, seconds: f64) {
    std::thread::spawn(move || {
        let outcome = run_try(&app, seconds);
        let _ = app.emit_to(
            WINDOW,
            "dictate-heard",
            match outcome {
                Ok(text) => Heard { text: Some(text), error: None },
                Err(error) => Heard { text: None, error: Some(error) },
            },
        );
    });
}

fn run_try(app: &AppHandle, seconds: f64) -> Result<String, String> {
    // The mark goes up before the microphone opens, so there is something to
    // see during the moment the device takes to start.
    let at = crate::hook::cursor_point();
    indicator::show(app, at);
    let pumping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    indicator::follow(app.clone(), pumping.clone());
    let _ = app.emit_to(WINDOW, "dictate-listening", true);

    let done = |p: &std::sync::Arc<std::sync::atomic::AtomicBool>| {
        p.store(false, std::sync::atomic::Ordering::SeqCst);
        indicator::hide(app);
    };

    match capture::start() {
        Err(e) => {
            done(&pumping);
            let _ = app.emit_to(WINDOW, "dictate-listening", false);
            return Err(e);
        }
        Ok(started) => {
            crate::diag(&format!(
                "dictate: recording with \"{}\"{}",
                started.device,
                match &started.instead_of {
                    Some(missing) => format!(" (instead of \"{missing}\", not connected)"),
                    None => String::new(),
                }
            ));
            // The window says which microphone was actually used, so a locked
            // one being absent is visible rather than only in the transcript.
            let _ = app.emit_to(WINDOW, "dictate-using", started);
        }
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds.clamp(0.5, 30.0)));
    done(&pumping);
    let _ = app.emit_to(WINDOW, "dictate-listening", false);

    let Some(wav) = capture::stop(&scratch_wav())? else {
        return Err("nothing was recorded".into());
    };
    let began = std::time::Instant::now();
    let text = engine::transcribe(&wav)?;
    crate::diag(&format!(
        "dictate: transcribed in {:.1}s: {} chars",
        began.elapsed().as_secs_f32(),
        text.len()
    ));
    let _ = std::fs::remove_file(&wav);
    if text.is_empty() {
        return Err("nothing was said, or the microphone heard nothing".into());
    }
    Ok(text)
}

/// A hold shorter than this is a brush against the key, not an intention to
/// speak. Measured on this machine: deliberate taps came in at 134-282 ms and
/// deliberate holds at 2462-4727 ms, so the gap is wide and this sits in it.
pub const MIN_HOLD: std::time::Duration = std::time::Duration::from_millis(400);

/// The trigger went down. Opens the microphone at once — the threshold is
/// applied on release, so that the start of a sentence is not clipped while
/// waiting to find out whether the hold was deliberate.
pub fn begin(app: &AppHandle, at: (f64, f64)) {
    if !ready() {
        crate::diag("dictate: trigger held, but the speech model is not downloaded");
        return;
    }
    indicator::show(app, at);
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    *FOLLOWING.lock().unwrap() = Some(running.clone());
    indicator::follow(app.clone(), running);
    if let Err(e) = capture::start() {
        crate::diag(&format!("dictate: could not start listening: {e}"));
        end_indicator(app);
    }
}

/// The trigger came up. Transcribes and pastes, on a thread of its own: this
/// is called from the hook's worker, which must not be held for the second or
/// two that transcription takes.
pub fn end(app: &AppHandle, held: std::time::Duration) {
    end_indicator(app);
    let app = app.clone();
    std::thread::spawn(move || {
        let short = held < MIN_HOLD;
        match capture::stop(&scratch_wav()) {
            Ok(Some(wav)) => {
                if short {
                    crate::diag(&format!(
                        "dictate: held {} ms — too short, discarded",
                        held.as_millis()
                    ));
                    let _ = std::fs::remove_file(&wav);
                    return;
                }
                let began = std::time::Instant::now();
                match engine::transcribe(&wav) {
                    Ok(text) if !text.is_empty() => {
                        crate::diag(&format!(
                            "dictate: transcribed in {:.1}s: {} chars, pasting",
                            began.elapsed().as_secs_f32(),
                            text.len()
                        ));
                        // The raw transcript goes to the Dictations jotpad
                        // before it is pasted, so a word lost between here and
                        // the document is still findable. The control panel
                        // does the writing: it owns `jotter.json`, and a second
                        // writer here would have its append saved over by
                        // whatever the editor next wrote.
                        let _ = app.emit_to("main", "dictate-transcript", text.clone());
                        crate::hook::paste_text(&text);
                    }
                    Ok(_) => crate::diag("dictate: nothing was heard"),
                    Err(e) => crate::diag(&format!("dictate: {e}")),
                }
                let _ = std::fs::remove_file(&wav);
            }
            Ok(None) => {}
            Err(e) => crate::diag(&format!("dictate: {e}")),
        }
    });
}

/// The follower for the recording in progress, so a release can retire it.
static FOLLOWING: Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> =
    Mutex::new(None);

fn end_indicator(app: &AppHandle) {
    if let Some(f) = FOLLOWING.lock().unwrap().take() {
        f.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    indicator::hide(app);
}

/// Every microphone on this Mac, and which one dictation will use.
#[derive(Serialize)]
pub struct Mics {
    devices: Vec<capture::MicInfo>,
    /// The chosen device's id, or `None` for "follow the system default".
    chosen: Option<String>,
    locked: bool,
}

#[tauri::command]
pub fn dictate_mics() -> Mics {
    let c = capture::choice();
    Mics {
        devices: capture::devices(),
        chosen: c.device,
        locked: c.locked,
    }
}

/// Choose a microphone. `device` of `None` means follow the system default,
/// which is what CQ does until told otherwise.
#[tauri::command]
pub fn dictate_set_mic(device: Option<String>, locked: bool) -> Result<(), String> {
    let name = device.as_deref().and_then(|id| {
        capture::devices()
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.name)
    });
    capture::set_choice(&capture::MicChoice { device, name, locked })
}

/// One microphone, as the tray menu needs it: already labelled, because a
/// menu cannot work out for itself that two devices share a name.
pub struct MicMenuItem {
    pub id: String,
    pub label: String,
}

pub struct MicMenu {
    pub devices: Vec<MicMenuItem>,
    pub chosen: Option<String>,
    pub locked: bool,
}

/// The microphone list for the tray. Labels match the window's: the maker is
/// added only where two devices would otherwise read identically, and the
/// system default is marked.
pub fn mic_menu() -> MicMenu {
    let all = capture::devices();
    let devices = all
        .iter()
        .map(|m| {
            let shared = all.iter().filter(|o| o.name == m.name).count() > 1;
            let base = match (shared, m.maker.as_deref()) {
                (true, Some(maker)) => format!("{} ({maker})", m.name),
                _ => m.name.clone(),
            };
            MicMenuItem {
                id: m.id.clone(),
                label: if m.is_default {
                    format!("{base} \u{2014} system default")
                } else {
                    base
                },
            }
        })
        .collect();
    let c = capture::choice();
    MicMenu { devices, chosen: c.device, locked: c.locked }
}

/// Act on a tray choice. `id` is the device id, or empty for "follow the
/// system default".
pub fn choose_mic(id: &str) -> Result<(), String> {
    let device = if id.is_empty() { None } else { Some(id.to_string()) };
    let name = device.as_deref().and_then(|d| {
        capture::devices().into_iter().find(|m| m.id == d).map(|m| m.name)
    });
    // Following the system default and locking contradict each other, so
    // choosing it releases the lock rather than leaving it set on nothing.
    let locked = device.is_some() && capture::choice().locked;
    capture::set_choice(&capture::MicChoice { device, name, locked })
}

pub fn toggle_mic_lock() -> Result<(), String> {
    let mut c = capture::choice();
    if c.device.is_none() {
        return Ok(()); // nothing to lock to
    }
    c.locked = !c.locked;
    capture::set_choice(&c)
}

/// Let go of the engine and the memory its model occupies.
#[tauri::command]
pub fn dictate_release() {
    engine::stop();
}

#[tauri::command]
pub fn dictate_models() -> Vec<ModelStatus> {
    status()
}

#[tauri::command]
pub fn dictate_download(app: AppHandle) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return; // already going
    }
    CANCEL.store(false, Ordering::SeqCst);
    std::thread::spawn(move || {
        fetch_all(&app);
        BUSY.store(false, Ordering::SeqCst);
    });
}

#[tauri::command]
pub fn dictate_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
    if let Some(mut c) = CHILD.lock().unwrap().take() {
        let _ = c.kill();
    }
    // The `.part` file is left where it is on purpose: the next download
    // resumes from it rather than fetching the same bytes twice.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The threshold that separates a brush against the key from an intention
    /// to speak, taken from what this keyboard actually produced.
    #[test]
    fn the_hold_threshold_sits_between_a_tap_and_a_hold() {
        use std::time::Duration;
        // Deliberate taps, measured: right Cmd 282 ms, right Shift 161 ms,
        // right Option 134 ms, left Option 127 ms.
        for tap in [127, 134, 161, 282] {
            assert!(
                Duration::from_millis(tap) < MIN_HOLD,
                "{tap} ms was a deliberate tap and must not dictate"
            );
        }
        // Deliberate holds, measured on the same keyboard.
        for hold in [2462, 4725, 4727] {
            assert!(
                Duration::from_millis(hold) > MIN_HOLD,
                "{hold} ms was a deliberate hold and must dictate"
            );
        }
    }

    #[test]
    fn the_model_table_is_sane() {
        for s in MODELS {
            assert!(!s.id.is_empty() && !s.file.is_empty());
            assert!(s.url.starts_with("https://"), "{} is not https", s.id);
            assert_eq!(s.sha256.len(), 64, "{} has a malformed sha256", s.id);
            assert!(
                s.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} has a non-hex sha256",
                s.id
            );
            assert!(s.bytes > 0);
        }
        let mut ids: Vec<_> = MODELS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MODELS.len(), "two models share an id");
    }

    #[test]
    fn resume_plan_covers_every_size() {
        let total = 100;
        assert_eq!(resume_plan(0, total), Plan::Fresh);
        assert_eq!(resume_plan(1, total), Plan::Resume);
        assert_eq!(resume_plan(99, total), Plan::Resume);
        // All the bytes are there, but only the checksum can say they are the
        // right ones, so this is never treated as finished.
        assert_eq!(resume_plan(100, total), Plan::Verify);
        // Longer than the model: appending would make it worse.
        assert_eq!(resume_plan(101, total), Plan::Restart);
    }

    /// Always on, so that each `--retry` attempt continues rather than
    /// truncating and starting again. Verified against the real CDN: on a file
    /// that is not there yet, curl starts at zero.
    #[test]
    fn curl_always_resumes() {
        let a = curl_args("https://example.com/m", Path::new("/tmp/m.part"));
        let at = a.iter().position(|x| x == "--continue-at").unwrap();
        assert_eq!(a[at + 1], "-");
        // Retrying is pointless if each attempt throws away the last one's work.
        assert!(a.iter().any(|x| x == "--retry"));
        assert!(at < a.iter().position(|x| x == "--output").unwrap());
    }

    #[test]
    fn curl_treats_an_error_page_as_a_failure() {
        // Without this flag a 404 body is written to the file and curl exits 0.
        let a = curl_args("https://example.com/m", Path::new("/tmp/m.part"));
        assert!(a.iter().any(|x| x == "--fail-with-body"));
        assert!(a.iter().any(|x| x == "--location"), "HF redirects to a CDN");
        // The URL goes last, after the options, and the output path is named.
        assert_eq!(a.last().unwrap(), "https://example.com/m");
        let o = a.iter().position(|x| x == "--output").unwrap();
        assert_eq!(a[o + 1], "/tmp/m.part");
    }

    #[test]
    fn sha256_matches_a_known_value() {
        // Guards against the hex formatting silently losing a leading zero.
        let dir = std::env::temp_dir().join(format!("cq-dictate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_of(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
