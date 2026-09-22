//! What was dictated, kept where the app can find it again.
//!
//! Every dictation is written here: what whisper heard, what the clean-up made
//! of it, and which of the two was pasted. It is not shown anywhere — the
//! jotpad this replaced was visible, and being visible was the problem, since
//! a list of raw transcripts is not something anyone wants among their notes.
//!
//! Written by Rust rather than by the control panel. The jotpad version had to
//! send the transcript to a window and hope it was alive to receive it; this
//! has no such dependency, which matters for the one job it has.
//!
//! ## It is a recording of someone speaking
//!
//! Plain text, on disk, of everything ever said into CQ, and now out of sight
//! where nobody will notice it growing. So it is capped rather than endless —
//! a few hundred dictations, the oldest dropped — and the file is named plainly
//! enough that someone looking through the app's folder can tell what it is.

use std::io::Write;
use std::path::PathBuf;

/// Roughly a few hundred dictations. Enough to answer "what did I actually
/// say" and to look into a rewrite that went wrong; not a transcript of every
/// word spoken into this machine for the rest of its life.
const MAX_BYTES: u64 = 512 * 1024;

pub fn file() -> PathBuf {
    crate::data_dir().join("dictations.log")
}

/// One dictation, as a line of JSON: readable by eye, greppable, and parseable
/// without a schema anyone has to maintain.
pub fn line(at: u64, heard: &str, cleaned: Option<&str>, pasted: &str, note: Option<&str>) -> String {
    let e = serde_json::json!({
        "at": at,
        "heard": heard,
        "cleaned": cleaned,
        "pasted": if pasted == heard { "heard" } else { "cleaned" },
        "note": note,
    });
    e.to_string()
}

/// Record a dictation. Never fails loudly: losing the record is a smaller
/// problem than interrupting the paste it belongs to.
pub fn write(heard: &str, cleaned: Option<&str>, pasted: &str, note: Option<&str>) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    roll_if_big(&path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", line(at, heard, cleaned, pasted, note));
    }
}

/// Keep one previous generation, as the diagnostics log does. Renaming rather
/// than truncating means the older half survives until the newer one fills.
fn roll_if_big(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_BYTES {
        return;
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("each line must be valid JSON on its own")
    }

    #[test]
    fn a_line_holds_both_versions_and_says_which_was_used() {
        let v = parsed(&line(1700, "meet tomorrow no Thursday", Some("meet Thursday"), "meet Thursday", None));
        assert_eq!(v["heard"], "meet tomorrow no Thursday");
        assert_eq!(v["cleaned"], "meet Thursday");
        assert_eq!(v["pasted"], "cleaned");
        assert_eq!(v["at"], 1700);
    }

    #[test]
    fn a_refused_rewrite_is_recorded_with_its_reason() {
        // The pair plus the reason is the whole point: "the rewrite ate my
        // sentence" cannot be checked if only one of them was kept.
        let v = parsed(&line(1, "a b c", Some("a"), "a b c", Some("LostSentence(\"b c\")")));
        assert_eq!(v["pasted"], "heard");
        assert_eq!(v["cleaned"], "a");
        assert!(v["note"].as_str().unwrap().contains("LostSentence"));
    }

    #[test]
    fn no_rewrite_at_all_is_not_confused_with_a_refused_one() {
        // Shift held, or the model not downloaded: there is no cleaned version,
        // which is different from having one and rejecting it.
        let v = parsed(&line(1, "as I said it", None, "as I said it", None));
        assert!(v["cleaned"].is_null());
        assert_eq!(v["pasted"], "heard");
        assert!(v["note"].is_null());
    }

    #[test]
    fn speech_with_quotes_and_newlines_stays_one_line() {
        // One dictation per line is what makes the file greppable; a quote or
        // a newline in what was said must not break that.
        let messy = "she said \"ship it\"\nand then left";
        let out = line(1, messy, None, messy, None);
        assert!(!out.trim_end().contains('\n'), "a record spilled onto a second line");
        assert_eq!(parsed(&out)["heard"], messy);
    }
}
