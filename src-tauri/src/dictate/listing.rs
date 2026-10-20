//! Turning a spoken list into a written one.
//!
//! Say "I need some things from the store. Eggs, milk, and bread." and whisper
//! hands back one paragraph. This turns it into the list it was meant to be:
//!
//! ```text
//! I need some things from the store:
//!
//! - Eggs
//! - milk
//! - bread
//! ```
//!
//! ## The model judges; this code edits
//!
//! Deciding whether something is a list is a judgement, and a judgement is
//! what a model is for. Writing the result is not: measured against the real
//! 7B, asked to produce formatted text it deleted lead-in sentences, reordered
//! items and invented a "second," that had never been said — 4 right out of 7.
//!
//! That measurement was taken as "the model cannot do this", and a pile of
//! deterministic rules went in instead: counting words, ordinals, conjunction
//! pairs, comma arithmetic. Every one of them had a case it got wrong, and each
//! fix needed another rule. The measurement had been of the wrong thing. The
//! model was being asked to *edit*, which it is bad at, when the hard part was
//! to *decide*, which it is good at.
//!
//! Asked only to decide — "is this a list, and which spans are the items?" —
//! the same model scored **9 out of 9** on the cases the rules needed special
//! handling for, including the two that no structural rule could separate:
//!
//! ```text
//! "I need some things from the store. Eggs, milk, and bread."  -> three items
//! "I'll be there soon. Traffic is bad, and I'm running late."  -> NONE
//! ```
//!
//! No counting words, no vocabulary of nouns, no verb list. So the model names
//! the items and **this code builds the output**, which means nothing it says
//! can reach the user's document as text.
//!
//! ## What is checked, and the hole that found
//!
//! Every item has to be a **literal substring of the input, in order and not
//! overlapping**. That alone is not enough. Given "a loaf of bread, some milk,
//! a dozen eggs" the model answered `bread`, `milk`, `eggs` — all three are
//! substrings in order, and "a loaf of" had quietly gone.
//!
//! So the gaps are checked too: between one item and the next there may be
//! punctuation and joining words, and nothing else. `, some ` fails that, which
//! is what catches the narrowing. [`BETWEEN`] is a word list, but it is only in
//! the verifier — it decides nothing about what is or is not a list.

use std::ops::Range;

use super::rewrite;

/// What the model is asked. It decides and names spans; it never writes output.
pub const INSTRUCTION: &str = "\
You are given a piece of dictated text. Decide whether it lists two or more separate items — things, not statements.

If it does, reply with the items, one per line, copying each item exactly as it appears in the text: the same words in the same order, nothing added, nothing shortened. Copy every word belonging to the item, including anything in front of it like \"a\", \"some\", \"the\", \"a loaf of\" or \"a dozen\". Leave out only the words that introduce the whole list or count the items off, like \"first\" or \"and\".

If it is ordinary prose, or a sequence of actions, or a single statement, reply with the one word NONE.

Reply with nothing else.";

/// Words allowed to sit between one item and the next.
///
/// Only the verifier uses this. Whether something is a list is the model's
/// call; this is how we check it did not swallow part of an item while
/// answering — the difference between `, and ` between two items, which is
/// fine, and `, some `, which means a word went missing.
const BETWEEN: &[&str] = &[
    "and", "or", "plus", "then", "also", "next", "first", "firstly", "second", "secondly",
    "third", "thirdly", "fourth", "fourthly", "fifth", "sixth", "seventh", "eighth", "ninth",
    "tenth", "finally", "lastly", "another", "thing", "well", "um", "uh",
];

/// Words that may be stripped off the end of a lead-in, because they belong
/// to the first item rather than to the sentence introducing it: "…from the
/// store. **First,** eggs".
///
/// Smaller than [`BETWEEN`] on purpose. That list holds "thing", for "another
/// thing", and stripping it here would turn "The first thing." into "The" —
/// the lead-in has to keep its own nouns.
const LEAD_TRAIL: &[&str] = &[
    "and", "or", "plus", "then", "also", "next", "first", "firstly", "second", "secondly",
    "third", "thirdly", "fourth", "fourthly", "fifth", "sixth", "seventh", "eighth", "ninth",
    "tenth", "finally", "lastly", "another",
];

/// Is there anything here that could be an enumeration at all?
///
/// A second model pass costs 300 ms to 1.9 s measured, and most dictations are
/// one sentence with nothing to list. This avoids paying for those. It makes no
/// judgement about what is a list — it only declines to ask about text with no
/// separator in it anywhere, which no answer could turn into one.
pub fn worth_asking(text: &str) -> bool {
    text.split_whitespace().count() >= 4 && (text.contains(',') || text.contains(';'))
}

/// The words of `text`, lowercased, punctuation dropped.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Read the model's reply into items, or `None` if it said NONE.
pub fn parse_reply(reply: &str) -> Option<Vec<String>> {
    let body = reply.trim();
    if body.is_empty() || body.to_uppercase().starts_with("NONE") {
        return None;
    }
    let items: Vec<String> = body
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    (items.len() >= 2).then_some(items)
}

/// Find each item in the text, in order and not overlapping.
///
/// Case-insensitive, because the model is copying from a sentence and may have
/// taken the capital off the first word.
pub fn locate(text: &str, items: &[String]) -> Option<Vec<Range<usize>>> {
    let haystack = text.to_lowercase();
    let mut spans = Vec::with_capacity(items.len());
    let mut from = 0;
    for item in items {
        let needle = item.to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let at = haystack.get(from..)?.find(&needle)? + from;
        // Must land on a character boundary of the original, which it does
        // when the lowercase mapping is length-preserving — and when it is
        // not, this is the check that notices.
        if !text.is_char_boundary(at) || !text.is_char_boundary(at + needle.len()) {
            return None;
        }
        spans.push(at..at + needle.len());
        from = at + needle.len();
    }
    Some(spans)
}

/// Between one item and the next, is there nothing but punctuation and joining
/// words?
///
/// This is what catches an item the model narrowed. Everything before the first
/// item is the lead-in and may be anything; everything after the last must be
/// punctuation only, so a list cannot swallow the sentence that followed it.
pub fn gaps_are_clean(text: &str, spans: &[Range<usize>]) -> bool {
    for pair in spans.windows(2) {
        let gap = &text[pair[0].end..pair[1].start];
        if !words(gap).iter().all(|w| BETWEEN.contains(&w.as_str())) {
            return false;
        }
    }
    match spans.last() {
        Some(last) => words(&text[last.end..]).is_empty(),
        None => false,
    }
}

/// Build the list. Only ever from spans of the input, so no wording of the
/// model's can reach the document.
pub fn render(text: &str, spans: &[Range<usize>]) -> String {
    // The lead-in keeps its own words and takes a colon. Whatever ended it —
    // a full stop, the comma before the first item, the ordinal that counted
    // the item off — comes away.
    let mut head = text[..spans[0].start].trim();
    loop {
        let trimmed = head.trim_end_matches([',', '.', ':', ';', '-', ' ']).trim_end();
        let last = words(trimmed).pop();
        match last {
            Some(w) if LEAD_TRAIL.contains(&w.as_str()) => {
                // Drop that word and go round again: "store. And third," has
                // two to lose.
                let cut = trimmed.len() - trimmed.len().min(
                    trimmed
                        .char_indices()
                        .rev()
                        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                        .count(),
                );
                head = trimmed[..cut].trim_end();
            }
            _ => {
                head = trimmed;
                break;
            }
        }
    }
    let head = head;
    let mut out = String::with_capacity(text.len() + 4 * spans.len());
    if !head.is_empty() {
        out.push_str(head);
        out.push_str(":\n\n");
    }
    for (i, span) in spans.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("- ");
        out.push_str(text[span.clone()].trim().trim_end_matches(['.', ',', ';']));
    }
    out
}

/// Turn a dictation into a list, if it is one and the feature is on.
pub fn format(text: &str) -> Option<String> {
    if !enabled() || !worth_asking(text) {
        return None;
    }
    let began = std::time::Instant::now();
    let budget = (words(text).len() * 3 + 48) as u32;
    let reply = match rewrite::ask_with(INSTRUCTION, text, budget) {
        Ok(r) => r,
        Err(e) => {
            crate::diag(&format!("dictate: no list check ({e})"));
            return None;
        }
    };
    let items = parse_reply(&reply)?;
    let Some(spans) = locate(text, &items) else {
        crate::diag("dictate: the listed items are not in what was said — left as one line");
        return None;
    };
    if !gaps_are_clean(text, &spans) {
        crate::diag("dictate: listing would have dropped something — left as one line");
        return None;
    }
    let out = render(text, &spans);
    crate::diag(&format!(
        "dictate: listed {} items in {:.0} ms",
        spans.len(),
        began.elapsed().as_secs_f32() * 1000.0
    ));
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

    /// The whole pipeline, with the model's part supplied by hand. Every reply
    /// here is one the real 7B actually gave.
    fn run(text: &str, reply: &str) -> Option<String> {
        let items = parse_reply(reply)?;
        let spans = locate(text, &items)?;
        gaps_are_clean(text, &spans).then(|| render(text, &spans))
    }

    #[test]
    fn a_spoken_list_becomes_a_written_one() {
        let had = "I need three things from the store. Eggs, milk, and bread.";
        assert_eq!(
            run(had, "Eggs\nmilk\nbread").unwrap(),
            "I need three things from the store:\n\n- Eggs\n- milk\n- bread"
        );
    }

    #[test]
    fn no_counting_word_is_needed() {
        // The case the deterministic rules could not reach without a number.
        let had = "I need some things from the store. Eggs, milk, and bread.";
        assert_eq!(
            run(had, "Eggs\nmilk\nbread").unwrap(),
            "I need some things from the store:\n\n- Eggs\n- milk\n- bread"
        );
    }

    #[test]
    fn ordinals_spoken_out_loud_do_not_come_along() {
        let had = "Three things I need from the store. First, eggs. Second, milk. And third, bread.";
        assert_eq!(
            run(had, "eggs\nmilk\nbread").unwrap(),
            "Three things I need from the store:\n\n- eggs\n- milk\n- bread"
        );
    }

    #[test]
    fn ten_items_are_no_different_from_three() {
        let had = "Grab a loaf of bread, some milk, a dozen eggs, butter, cheese, apples, \
                   coffee, tea, sugar, and salt.";
        let reply = "a loaf of bread\nsome milk\na dozen eggs\nbutter\ncheese\napples\ncoffee\n\
                     tea\nsugar\nsalt";
        let out = run(had, reply).unwrap();
        assert_eq!(out.lines().filter(|l| l.starts_with("- ")).count(), 10);
        assert!(out.starts_with("Grab:\n\n- a loaf of bread\n"), "{out:?}");
    }

    #[test]
    fn an_item_the_model_narrowed_is_refused() {
        // The real failure. All three are substrings in order, and "a loaf of"
        // has gone — which "substring, in order" cannot see and the gap check
        // can: between "bread" and "milk" sits ", some ".
        let had = "Grab a loaf of bread, some milk, and a dozen eggs.";
        let items = parse_reply("bread\nmilk\neggs").unwrap();
        let spans = locate(had, &items).unwrap();
        assert!(!gaps_are_clean(had, &spans), "the narrowing must be caught");
    }

    #[test]
    fn a_list_may_not_swallow_the_sentence_after_it() {
        // Anything but punctuation after the last item means the model stopped
        // early and the rest would be lost.
        let had = "I need things. Eggs, milk, and bread. Also get wine.";
        let items = parse_reply("Eggs\nmilk\nbread").unwrap();
        let spans = locate(had, &items).unwrap();
        assert!(!gaps_are_clean(had, &spans));
    }

    #[test]
    fn invented_items_are_refused() {
        let had = "I need three things from the store. Eggs, milk, and bread.";
        let items = parse_reply("Eggs\nmilk\ncheese").unwrap();
        assert!(locate(had, &items).is_none(), "cheese was never said");
    }

    #[test]
    fn reordered_items_are_refused() {
        let had = "I need three things from the store. Eggs, milk, and bread.";
        let items = parse_reply("milk\nEggs\nbread").unwrap();
        assert!(locate(had, &items).is_none(), "out of order");
    }

    #[test]
    fn none_means_leave_it_alone() {
        for reply in ["NONE", "NONE.", "none", "  NONE  ", ""] {
            assert!(parse_reply(reply).is_none(), "{reply:?}");
        }
        // And one item is not a list.
        assert!(parse_reply("eggs").is_none());
    }

    #[test]
    fn bullets_the_model_adds_itself_are_stripped() {
        let items = parse_reply("- Eggs\n* milk\n• bread").unwrap();
        assert_eq!(items, ["Eggs", "milk", "bread"]);
    }

    #[test]
    fn a_list_with_no_lead_in_gets_no_colon() {
        let had = "Eggs, milk, and bread.";
        assert_eq!(run(had, "Eggs\nmilk\nbread").unwrap(), "- Eggs\n- milk\n- bread");
    }

    #[test]
    fn the_model_is_not_asked_about_text_with_nothing_to_list() {
        // The pre-filter. It judges nothing — it only declines to pay for a
        // model call on text with no separator in it at all.
        assert!(!worth_asking("Testing testing."));
        assert!(!worth_asking("Eggs, milk."), "too short to be worth a pass");
        assert!(worth_asking("I need things. Eggs, milk, and bread."));
        assert!(worth_asking("So, um, I think we should, you know, just ship it."));
    }

    #[test]
    fn a_lead_in_keeps_its_own_nouns() {
        // "thing" is a joining word in "another thing" but a noun here, and
        // stripping it would leave the lead-in as "The".
        let had = "The first thing I need. Eggs, milk, and bread.";
        let out = run(had, "Eggs\nmilk\nbread").unwrap();
        assert!(out.starts_with("The first thing I need:"), "{out:?}");
    }

    #[test]
    fn a_lead_in_loses_every_trailing_ordinal() {
        // "store. And third," has two words to lose, not one.
        let had = "Things I need from the store. And third, bread, cheese, and milk.";
        let out = run(had, "bread\ncheese\nmilk").unwrap();
        assert!(out.starts_with("Things I need from the store:"), "{out:?}");
    }

    #[test]
    fn the_instruction_asks_for_whole_items() {
        // Measured: without this the model answered "bread" for "a loaf of
        // bread", which the gap check then refused — a safe failure, but a
        // failure. Saying it plainly turned 9/10 into 10/10.
        let i = INSTRUCTION.to_lowercase();
        assert!(i.contains("nothing shortened"));
        assert!(i.contains("every word belonging to the item"));
        assert!(i.contains("none"), "it must have a way to decline");
    }

    #[test]
    fn multibyte_items_are_located_on_character_boundaries() {
        let had = "Café things. The ☕ machine, the 🙂 sign, and the door.";
        let out = run(had, "The ☕ machine\nthe 🙂 sign\nthe door").unwrap();
        assert_eq!(out, "Café things:\n\n- The ☕ machine\n- the 🙂 sign\n- the door");
    }

    #[test]
    fn an_underscore_in_an_item_survives() {
        // Whisper's vocabulary prompt writes these, and nothing here may
        // break them: `words` keeps the underscore for exactly that reason.
        let had = "Two columns to exclude. location_id, and session_id.";
        let out = run(had, "location_id\nsession_id").unwrap();
        assert_eq!(out, "Two columns to exclude:\n\n- location_id\n- session_id");
    }
}
