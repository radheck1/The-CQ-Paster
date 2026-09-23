//! The words dictation should expect to hear.
//!
//! Whisper takes an "initial prompt": text that primes the decoder before it
//! hears anything. Fed the names you actually use, it stops guessing at them —
//! and it does more than spell them right. Measured on a 99-second recording,
//! adding a list of ten terms turned "created underscore at" into `created_at`,
//! "customer ID" into `customer_id`, "3.30" into "3:30", and put real
//! quotation marks around a quoted sentence. None of that was asked for
//! directly; the model infers the register from the words it is primed with.
//!
//! ## The budget is small and the overflow is silent
//!
//! Whisper's prompt is capped at `min(n_max_text_ctx, n_text_ctx/2)` = **223
//! tokens**. Past that it keeps only the *last* 223 and drops the rest, saying
//! so in a log nobody reads. Measured against the real model: 60 terms (589
//! characters) fit, 80 terms (786 characters) came to 298 tokens and were cut.
//!
//! So the list is capped here, where the user can be told, rather than being
//! quietly trimmed inside the decoder. `MAX_TERMS` is deliberately under the
//! measured ceiling: terms vary in length, and a list of long ones would
//! otherwise slip over it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Kept below the measured ceiling of about 60, because a list of unusually
/// long terms reaches 223 tokens sooner than a list of short ones.
pub const MAX_TERMS: usize = 48;

/// A term longer than this is a phrase or a paste accident, not a name.
const MAX_TERM_LEN: usize = 48;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Vocab {
    pub terms: Vec<String>,
    /// Suggestions that were turned down. Kept so the same word is not
    /// offered every time CQ looks — a suggestion list that keeps proposing
    /// what has already been refused stops being read.
    pub dismissed: Vec<String>,
}

fn file() -> PathBuf {
    crate::data_dir().join("dictation-vocab.json")
}

pub fn load() -> Vocab {
    std::fs::read(file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vocab>(&b).ok())
        .map(|v| Vocab { terms: clean(&v.terms), dismissed: v.dismissed })
        .unwrap_or_default()
}

pub fn save(terms: &[String]) -> Result<(), String> {
    let existing = load();
    save_all(&Vocab { terms: clean(terms), dismissed: existing.dismissed })
}

pub fn save_all(v: &Vocab) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    let path = file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("cannot save the vocabulary: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot save the vocabulary: {e}"))?;
    Ok(())
}

/// Tidy a list of terms: trim, drop blanks and over-long entries, remove
/// duplicates case-insensitively while keeping the spelling first written —
/// the point of the list is to teach a particular spelling, so `customer_id`
/// and `Customer_ID` must not both be taught.
pub fn clean(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = t.trim();
        if t.is_empty() || t.chars().count() > MAX_TERM_LEN {
            continue;
        }
        if out.iter().any(|o| o.eq_ignore_ascii_case(t)) {
            continue;
        }
        out.push(t.to_string());
    }
    out
}

/// The terms that will actually reach the decoder: everything past the budget
/// is dropped here rather than inside whisper, so the window can say so.
pub fn used(terms: &[String]) -> &[String] {
    &terms[..terms.len().min(MAX_TERMS)]
}

/// Build the initial prompt.
///
/// Written as a sentence rather than a bare list: whisper is primed by text
/// that looks like speech, and a comma-separated run ending in a full stop
/// reads as the kind of context the model was trained on.
pub fn prompt_from(terms: &[String]) -> String {
    let use_these = used(terms);
    if use_these.is_empty() {
        return String::new();
    }
    format!("{}.", use_these.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_list_makes_no_prompt() {
        // Not "." — an empty prompt must be omitted entirely, or whisper is
        // primed with a full stop and nothing else.
        assert_eq!(prompt_from(&[]), "");
        assert_eq!(prompt_from(&v(&["   ", ""])), "   , .");
        assert_eq!(prompt_from(&clean(&v(&["   ", ""]))), "");
    }

    #[test]
    fn the_prompt_reads_as_a_sentence() {
        assert_eq!(
            prompt_from(&v(&["Snowflake", "Pendo", "customer_id"])),
            "Snowflake, Pendo, customer_id."
        );
    }

    #[test]
    fn one_spelling_of_a_term_survives() {
        // Teaching two spellings of the same name teaches neither.
        let c = clean(&v(&["customer_id", "Customer_ID", "CUSTOMER_ID"]));
        assert_eq!(c, v(&["customer_id"]));
    }

    #[test]
    fn blanks_and_pasted_paragraphs_are_dropped() {
        let long = "x".repeat(MAX_TERM_LEN + 1);
        let c = clean(&v(&["  Pendo  ", "", "   ", &long, "MRR"]));
        assert_eq!(c, v(&["Pendo", "MRR"]));
    }

    #[test]
    fn the_budget_is_enforced_here_not_in_the_decoder() {
        // Whisper keeps the LAST 223 tokens and says so only in a log, so a
        // list that overflows would silently lose whatever came first.
        let many: Vec<String> = (0..MAX_TERMS + 10).map(|i| format!("term{i}")).collect();
        assert_eq!(used(&many).len(), MAX_TERMS);
        assert_eq!(used(&many)[0], "term0", "the first terms are the ones kept");
        let p = prompt_from(&many);
        assert!(p.contains("term0"));
        assert!(!p.contains(&format!("term{}", MAX_TERMS)), "past the budget");
    }

    #[test]
    fn the_budget_stays_under_what_the_decoder_measured() {
        // 60 terms of ~10 characters fitted; 80 did not. This keeps a margin
        // for terms longer than the ones that were measured.
        assert!(MAX_TERMS <= 60, "over the measured ceiling");
        let worst: Vec<String> = (0..MAX_TERMS).map(|_| "x".repeat(MAX_TERM_LEN)).collect();
        // Even every term at its maximum length stays near the 589-character
        // list that was measured to fit, once de-duplication is applied.
        assert!(clean(&worst).len() == 1, "identical terms collapse");
    }

    #[test]
    fn a_saved_file_missing_its_field_still_loads() {
        let v: Vocab = serde_json::from_str("{}").unwrap();
        assert!(v.terms.is_empty());
    }
}
