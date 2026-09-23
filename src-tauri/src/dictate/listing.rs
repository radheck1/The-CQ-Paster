//! Putting a spoken list onto separate lines.
//!
//! Say "three things I need from the store — first, milk; second, eggs; third,
//! bread" and whisper hands back one paragraph. This puts each item on a line
//! of its own:
//!
//! ```text
//! Three things I need from the store.
//! First, milk.
//! Second, eggs.
//! Third, some bread.
//! ```
//!
//! ## It does not ask the model, and the reason is measured
//!
//! The obvious design is a second pass through `llama-server` with an
//! instruction permitting only whitespace, guarded by an exact check — if the
//! whitespace-separated tokens match, not one word can have been lost. That
//! was built and tried against the real 7B. It does not work, and how it fails
//! is worth recording.
//!
//! On four real lists the model produced one correct answer. On the others it
//! **deleted the lead-in sentence** ("Three things I need from the store."
//! simply gone), stripped full stops from the items, and on one it reordered
//! the whole thing and invented a "second," that had never been said. That is
//! the sentence-loss failure `MACOS_PORT.md` records, reproducing on demand:
//! *a model rearranging text drops pieces*. The guard caught every one, so
//! nothing was pasted wrong — but almost nothing was formatted either.
//!
//! Worse, the guard turned out not to be enough on its own. Ordinary prose
//! came back cut into five pieces with every word intact, which passes an
//! exact token comparison and is plainly worse than what went in. **The words
//! being safe is not the text being safe.**
//!
//! Fixing that meant demanding that every line begin with the word counting it
//! off — and once that is the requirement, the transformation is mechanical.
//! There is nothing left for a model to decide. So this splits the text
//! itself:
//!
//! * no second pass, so no added wait on any dictation;
//! * nothing to refuse, so a list that should be split always is;
//! * it keeps the lead-in sentence, which the model kept throwing away;
//! * and it is a pure function, so every case below is a test rather than a
//!   sampling run against a 4.7 GB file.
//!
//! On the seven cases measured against the model — four real lists and three
//! prose paragraphs from the author's own log — the model scored 4/7 and this
//! scores 7/7.
//!
//! ## What it will not do
//!
//! "Two things, eggs and bacon" stays as it is. Its items carry no marker, so
//! splitting them would mean deleting the "and", and deleting words is the
//! thing that loses sentences. A list that cannot be split without editing is
//! left alone.

/// Words that begin an item, on their own.
const MARKERS: &[&str] = &[
    "first", "firstly", "second", "secondly", "third", "thirdly", "fourth", "fifth", "sixth",
    "seventh", "lastly", "finally", "next",
];

/// Pairs that begin an item. "and then" is how a list is counted off in
/// speech; "then" alone is not, since it is how any sequence is described —
/// "I finished the report, then I sent it over" is not a list.
const MARKER_PAIRS: &[(&str, &str)] = &[
    ("and", "then"),
    ("and", "finally"),
    ("and", "lastly"),
    ("another", "thing"),
];
// ("one", "more") was tried and removed: it split "Alright, one more test to
// see if it's working" — a plain sentence — into two lines. "another thing"
// covers the same intent without the collision.

/// How many breaks it takes before this is a list rather than a turn of
/// phrase.
///
/// Two, so three lines. One marker alone is ordinary speech far more often
/// than it is a list — "I finished the report. Next, I'll send it over" wants
/// no line break — and a real list counts off at least twice.
const MIN_BREAKS: usize = 2;

/// Marks that end a clause. A line break can only replace the space after one
/// of these, never fall inside a clause.
const CLAUSE_END: &[char] = &['.', '!', '?', ';', ','];

/// The first word or two of `s`, lowercased and stripped of punctuation.
fn opening_words(s: &str) -> (String, String) {
    let mut it = s.split_whitespace().map(|w| {
        w.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    });
    (it.next().unwrap_or_default(), it.next().unwrap_or_default())
}

/// Does the text at this point begin a listed item?
fn begins_an_item(s: &str) -> bool {
    let (one, two) = opening_words(s);
    if one.is_empty() {
        return false;
    }
    MARKER_PAIRS.iter().any(|(a, b)| one == *a && two == *b) || MARKERS.contains(&one.as_str())
}

/// Byte offsets where a clause begins: the start, and the first non-space
/// after each clause-ending mark.
fn clause_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut pending = false;
    for (i, c) in text.char_indices() {
        if pending && !c.is_whitespace() {
            starts.push(i);
            pending = false;
        }
        if CLAUSE_END.contains(&c) {
            pending = true;
        }
    }
    starts.dedup();
    starts
}

/// Put each counted-off item on its own line, or `None` if this is not a list.
///
/// Pure. The only edit it can make is turning the space before a marker into a
/// line break, which is why it cannot lose a word: there is no path here that
/// drops one.
pub fn split_into_items(text: &str) -> Option<String> {
    let starts = clause_starts(text);
    // The first clause is the lead-in and is never broken before: there is
    // nothing in front of it to break away from.
    let breaks: Vec<usize> = starts
        .iter()
        .skip(1)
        .copied()
        .filter(|&at| begins_an_item(&text[at..]))
        .collect();
    if breaks.len() < MIN_BREAKS {
        return None;
    }
    let mut out = String::with_capacity(text.len() + breaks.len());
    let mut from = 0;
    for at in breaks {
        out.push_str(text[from..at].trim_end());
        out.push('\n');
        from = at;
    }
    out.push_str(text[from..].trim_end());
    Some(out)
}

/// Did nothing but whitespace change?
///
/// Kept as a runtime check even though [`split_into_items`] cannot break it by
/// construction. Slicing a string by byte offsets is exactly the kind of code
/// that is correct until it is not, and the cost of being wrong here is a
/// mangled paste into the user's document. It costs one pass over the words.
pub fn only_whitespace_changed(before: &str, after: &str) -> bool {
    before.split_whitespace().eq(after.split_whitespace())
}

/// Format a dictation as a list, if it is one and the feature is on.
pub fn format(text: &str) -> Option<String> {
    if !enabled() {
        return None;
    }
    let out = split_into_items(text)?;
    if !only_whitespace_changed(text, &out) {
        // Unreachable unless the splitting above is wrong. If it ever is, the
        // original text goes in and the log says why.
        crate::diag("dictate: list split changed more than whitespace — refused");
        return None;
    }
    crate::diag(&format!("dictate: listed onto {} lines", out.lines().count()));
    Some(out)
}

// ---------------------------------------------------------------- the switch

fn setting_file() -> std::path::PathBuf {
    crate::data_dir().join("dictation-lists.json")
}

/// Is automatic list formatting on? On by default, and one checkbox from off.
///
/// A missing or unreadable file means on, matching how the microphone choice
/// treats a file it cannot read: a setting nobody has touched should behave
/// like the shipped default, not like a failure.
pub fn enabled() -> bool {
    std::fs::read(setting_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(true)
}

pub fn set_enabled(on: bool) -> Result<(), String> {
    let path = setting_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::json!({ "enabled": on }).to_string();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("cannot save the setting: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot save the setting: {e}"))?;
    crate::diag(&format!(
        "dictate: automatic list formatting turned {}",
        if on { "on" } else { "off" }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case here was run against the real 7B first. The model scored
    /// 4/7; these are the same seven.
    #[test]
    fn a_counted_off_list_goes_onto_lines() {
        assert_eq!(
            split_into_items(
                "First, fix the login page. Second, update the documentation. Third, ship it."
            )
            .unwrap(),
            "First, fix the login page.\nSecond, update the documentation.\nThird, ship it."
        );
    }

    #[test]
    fn the_lead_in_sentence_is_kept() {
        // The model deleted this one outright — "Three things I need from the
        // store." simply vanished. It is the sentence that says what the list
        // is, so losing it is the worst single thing that can happen here.
        assert_eq!(
            split_into_items(
                "Three things I need from the store. First, milk. Second, eggs. Third, some bread."
            )
            .unwrap(),
            "Three things I need from the store.\nFirst, milk.\nSecond, eggs.\nThird, some bread."
        );
    }

    #[test]
    fn and_then_counts_off_an_item() {
        // Real, from the log. The model reordered this one and invented a
        // "second," that was never said.
        let had = "Okay, two notes. First, for the audio bars, can we make them skinnier? \
                   And then the blob, it looks contained within a box.";
        let out = split_into_items(had).unwrap();
        assert_eq!(out.lines().count(), 3);
        assert!(out.lines().nth(1).unwrap().starts_with("First,"));
        assert!(out.lines().nth(2).unwrap().starts_with("And then"));
        assert!(only_whitespace_changed(had, &out));
    }

    #[test]
    fn prose_is_left_alone() {
        // All three from the real log. The model cut the second one into five
        // pieces, which passed an exact token comparison and was still wrong.
        for t in [
            "Two things I need you to work on adding to the dictation.",
            "Also, we need to incorporate the same functionality as Wispr Flow, where if I \
             start recording something using dictation and my cursor isn't anywhere where the \
             text can paste, then it should just be saved to my clipboard.",
            "There should be more settings in the tray bar under the shaken stuff. You should \
             be able to select a default tool that opens when shaken, like Jotter, and also a \
             few more options.",
        ] {
            assert!(split_into_items(t).is_none(), "{t:?}");
        }
    }

    #[test]
    fn a_list_that_cannot_be_split_without_editing_is_left_alone() {
        // Splitting "eggs and bacon" into items would mean deleting the "and",
        // and deleting words is what loses sentences.
        assert!(split_into_items("Two things, eggs and bacon.").is_none());
    }

    #[test]
    fn then_alone_is_a_sequence_not_a_list() {
        assert!(
            split_into_items("I finished the report, then I sent it over, then I went home.")
                .is_none()
        );
        assert!(split_into_items("I'll do that first, then I'll call you.").is_none());
    }

    #[test]
    fn one_marker_is_not_enough() {
        // A turn of phrase, not a list.
        assert!(split_into_items("I finished the report. Next, I'll send it over.").is_none());
    }

    #[test]
    fn a_marker_inside_a_clause_is_not_a_break() {
        // "first" here is an adjective, and "the second one" does not begin
        // with its marker.
        let t = "Can you give me access to edit the first Excel document, and the second one \
                 you sent me, please?";
        assert!(split_into_items(t).is_none());
    }

    #[test]
    fn nothing_is_ever_lost_or_added() {
        // The property that matters, asserted on every case that splits.
        for t in [
            "First, fix the login page. Second, update the docs. Third, ship it.",
            "Three things I need. First, milk. Second, eggs. Third, bread.",
            "Two notes. First, the bars. And then the blob.",
        ] {
            let out = split_into_items(t).unwrap();
            assert!(only_whitespace_changed(t, &out), "{t:?}");
            assert!(!out.contains("\n\n"), "blank line in {out:?}");
            assert!(
                !out.lines().any(|l| l.ends_with(' ')),
                "trailing space in {out:?}"
            );
        }
    }

    #[test]
    fn markers_are_found_regardless_of_case_or_punctuation() {
        assert!(begins_an_item("Second, update the docs"));
        assert!(begins_an_item("SECOND: update"));
        assert!(begins_an_item("and then the blob"));
        assert!(begins_an_item("Another thing, the logo"));
        assert!(!begins_an_item(""));
        assert!(!begins_an_item("the second thing"));
    }

    #[test]
    fn clause_starts_finds_every_clause_and_no_empty_ones() {
        let t = "One. Two, three; four! five?  six";
        let starts = clause_starts(t);
        let heads: Vec<&str> = starts
            .iter()
            .map(|&i| t[i..].split(' ').next().unwrap())
            .collect();
        assert_eq!(heads, ["One.", "Two,", "three;", "four!", "five?", "six"]);
    }

    #[test]
    fn text_with_no_clauses_at_all_is_safe() {
        assert!(split_into_items("").is_none());
        assert!(split_into_items("hello").is_none());
        assert!(split_into_items("...").is_none());
    }

    /// Runs the splitter over every dictation this machine has recorded and
    /// prints what it would do. Not an assertion — it is how the markers get
    /// tuned against real speech rather than against invented examples.
    ///
    /// Ignored by default: it reads a file that only exists on a machine that
    /// has actually dictated, and it prints what was said into it.
    #[test]
    #[ignore = "reads the local dictation log; run it by hand to tune the markers"]
    fn report_against_every_recorded_dictation() {
        let path = crate::data_dir().join("dictations.log");
        let Ok(body) = std::fs::read_to_string(&path) else {
            eprintln!("no dictation log at {}", path.display());
            return;
        };
        let (mut seen, mut split) = (0, 0);
        for line in body.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let heard = v.get("heard").and_then(|h| h.as_str()).unwrap_or("");
            let cleaned = v.get("cleaned").and_then(|c| c.as_str());
            let used = if v.get("pasted").and_then(|p| p.as_str()) == Some("cleaned") {
                cleaned.unwrap_or(heard)
            } else {
                heard
            };
            if used.is_empty() {
                continue;
            }
            seen += 1;
            if let Some(out) = split_into_items(used) {
                split += 1;
                assert!(only_whitespace_changed(used, &out), "corrupted: {used:?}");
                eprintln!("--- would split into {} lines:", out.lines().count());
                for l in out.lines() {
                    eprintln!("    | {l}");
                }
            }
        }
        eprintln!("\n==== {split} of {seen} dictations would be listed ====");
    }

    #[test]
    fn multibyte_text_is_sliced_on_character_boundaries() {
        // Byte offsets into a string are how this splits, so a café and an
        // emoji have to be safe to cut around.
        let t = "Café notes. First, the ☕ machine. Second, the 🙂 sign. Third, the door.";
        let out = split_into_items(t).unwrap();
        assert_eq!(out.lines().count(), 4);
        assert!(only_whitespace_changed(t, &out));
    }
}
