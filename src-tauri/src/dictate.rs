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
    crate::make_native_titlebar(app, WINDOW);
    let _ = win.show();
    let _ = win.set_focus();
}

#[tauri::command]
pub fn dictate_open(app: AppHandle) {
    open_window(&app);
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
