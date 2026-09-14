//! Storage for CQ Jotter (macOS only): every Jotter folder and its note, held as
//! one JSON document.
//!
//! The document's shape belongs to the frontend (`src/jotter.ts`); this module
//! only moves it to and from disk. It does make three guarantees, because a note
//! is the one thing in the app that can't be recaptured by copying it again:
//!
//! - **JSON, not bincode.** Slots use bincode, which is not self-describing: a
//!   field added to the format later silently invalidates every existing file.
//!   A new field in a JSON document is simply absent from an older one.
//! - **Atomic writes.** The document is written to a temporary file, flushed,
//!   and renamed over the old one, so a crash mid-save leaves the previous copy.
//! - **An unreadable file is set aside, never overwritten.** It is renamed next
//!   to the original with a timestamp, and the Jotter starts fresh.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serialises writes. The temporary file has a fixed name, so two saves racing
/// on Tauri's command threads could otherwise interleave.
static WRITE: Mutex<()> = Mutex::new(());

pub(crate) fn file() -> PathBuf {
    crate::data_dir().join("jotter.json")
}

/// The saved document, or `None` if there isn't one yet. An error means a file
/// exists but could not be read or moved aside; the frontend then declines to
/// save, so whatever is on disk survives.
#[tauri::command]
pub fn jotter_load() -> Result<Option<String>, String> {
    load(&file())
}

/// Save, then let the reminder schedule see the new settings and notes.
#[tauri::command]
pub fn jotter_save(app: tauri::AppHandle, doc: String) -> Result<(), String> {
    save(&file(), &doc)?;
    crate::reminders::update_doc(&app, &doc);
    Ok(())
}

pub fn load(path: &Path) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if let Ok(text) = String::from_utf8(bytes) {
        if serde_json::from_str::<serde_json::Value>(&text).is_ok() {
            return Ok(Some(text));
        }
    }
    let aside = set_aside_path(path);
    std::fs::rename(path, &aside)
        .map_err(|e| format!("cannot move unreadable {} aside: {e}", path.display()))?;
    // Not from tests: they exercise this with a scratch file, and `diag` writes
    // to the real app's log.
    if !cfg!(test) {
        crate::diag(&format!(
            "jotter: {} could not be read; kept it as {}",
            path.display(),
            aside.display()
        ));
    }
    Ok(None)
}

pub fn save(path: &Path, doc: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(doc)
        .map_err(|e| format!("refusing to save a document that is not JSON: {e}"))?;
    let _guard = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(doc.as_bytes())?;
        f.sync_all()
    };
    write().map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

fn set_aside_path(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    path.with_file_name(format!("jotter.unreadable-{secs}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty directory per test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cq-jotter-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let path = scratch("missing").join("jotter.json");
        assert_eq!(load(&path), Ok(None));
    }

    #[test]
    fn round_trips_and_leaves_no_temp_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("jotter.json");
        save(&path, r#"{"version":1,"folders":[]}"#).unwrap();
        save(&path, r#"{"version":1,"folders":[{"id":1}]}"#).unwrap();
        assert_eq!(load(&path).unwrap().as_deref(), Some(r#"{"version":1,"folders":[{"id":1}]}"#));
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("jotter.json")]);
    }

    #[test]
    fn refuses_to_save_non_json_and_keeps_the_old_file() {
        let path = scratch("nonjson").join("jotter.json");
        save(&path, r#"{"keep":true}"#).unwrap();
        assert!(save(&path, "not json").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"keep":true}"#);
    }

    #[test]
    fn unreadable_file_is_set_aside_with_its_bytes_intact() {
        let dir = scratch("corrupt");
        let path = dir.join("jotter.json");
        std::fs::write(&path, b"{\"trunc").unwrap();
        assert_eq!(load(&path), Ok(None));
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("jotter.unreadable-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read(&aside[0]).unwrap(), b"{\"trunc");
    }
}
