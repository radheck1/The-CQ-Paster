//! Finding the words worth teaching the speech model.
//!
//! The vocabulary list is what stops dictation guessing at names, and it is
//! worth more than spelling — priming the decoder with `customer_id` also
//! makes it write spoken "underscore" as one. But it has to be typed, and a
//! list nobody maintains goes stale.
//!
//! CQ already holds the raw material. The clipboard slots are full of things
//! copied out of a terminal or a query — column names, ticket ids, product
//! names — and the jotpads are full of things written by hand. Both are the
//! user's own words in the user's own spelling, which is exactly what a
//! vocabulary list is.
//!
//! Nothing here reaches the decoder on its own. Candidates are offered and the
//! user ticks them, because priming whisper with a **wrong** term actively
//! makes transcription worse: a bad automatic entry is not neutral, it is a
//! new error in every dictation that follows.
//!
//! The budget is 48 terms (`vocab::MAX_TERMS`), so this ranks and the user
//! chooses; it cannot simply accumulate.

use std::collections::HashMap;

/// A word has to be at least this long to be worth teaching. Shorter than this
/// and whisper has almost certainly seen it.
const MIN_LEN: usize = 3;
/// Longer than this is a hash, a token or a pasted line, not a name.
const MAX_LEN: usize = 32;

/// Words that look distinctive but are only common English wearing capitals —
/// the first word of a sentence, mostly. Without this every sentence start in
/// every note becomes a candidate.
const NOT_WORTH_IT: &[&str] = &[
    "the", "and", "but", "for", "not", "you", "all", "any", "can", "her", "was",
    "one", "our", "out", "day", "get", "has", "him", "his", "how", "its", "new",
    "now", "old", "see", "two", "way", "who", "boy", "did", "use", "man", "men",
    "put", "say", "she", "too", "yes", "yet", "this", "that", "with", "from",
    "they", "have", "what", "were", "when", "your", "said", "each", "which",
    "their", "will", "about", "would", "there", "could", "other", "into",
    "than", "then", "them", "some", "very", "just", "like", "also", "back",
    "after", "first", "well", "year", "work", "make", "over", "think", "only",
    "know", "take", "come", "good", "want", "does", "need", "here", "more",
    "most", "such", "these", "those", "still", "being", "where", "while",
];

/// Why a word looks like something the speaker would say and whisper would
/// get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `customer_id`, `fact_tickets` — an identifier, and the case whisper
    /// most reliably mangles, since it hears "customer I D".
    Identifier,
    /// `AccuLynx`, `TypeScript` — a capital inside the word.
    CamelCase,
    /// `MRR`, `CSV`, `API` — spoken as letters.
    Acronym,
    /// `whisper.cpp`, `llama.cpp` — a dot inside the word.
    Dotted,
    /// `Snowflake`, `Pendo` — capitalised where a sentence did not start.
    ProperNoun,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub term: String,
    pub why: Why,
    /// How often it was seen. Ranks the list; a name used once is probably
    /// not worth a place in a budget of 48.
    pub count: usize,
}

/// Strip what punctuation attaches to a word without being part of it.
/// Trailing dots are taken but inner ones kept, so `whisper.cpp` survives and
/// `Snowflake.` does not keep its full stop.
fn trim_word(w: &str) -> &str {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim_end_matches('.')
}

/// Does this word look like something worth teaching, and why?
///
/// `at_sentence_start` is passed in rather than guessed: a capitalised word is
/// only evidence of a name when a sentence did not just begin.
pub fn classify(word: &str, at_sentence_start: bool) -> Option<Why> {
    let w = trim_word(word);
    let n = w.chars().count();
    if n < MIN_LEN || n > MAX_LEN {
        return None;
    }
    if !w.chars().next().is_some_and(|c| c.is_alphabetic()) {
        return None; // numbers, ids, anything starting with punctuation
    }
    if NOT_WORTH_IT.contains(&w.to_lowercase().as_str()) {
        return None;
    }
    if w.contains('_') && w.chars().any(|c| c.is_alphabetic()) {
        return Some(Why::Identifier);
    }
    if w.contains('.') && w.chars().filter(|c| *c == '.').count() == 1 {
        let (a, b) = w.split_once('.').unwrap();
        if a.len() >= 2 && b.len() >= 2 && b.chars().all(|c| c.is_alphanumeric()) {
            return Some(Why::Dotted);
        }
    }
    if n <= 6 && w.chars().all(|c| c.is_uppercase() || c.is_numeric()) && w.chars().any(|c| c.is_alphabetic()) {
        return Some(Why::Acronym);
    }
    // A capital inside the word: AccuLynx, TypeScript. Checked before the
    // plain proper-noun case, which is much weaker evidence.
    if w.chars().skip(1).any(|c| c.is_uppercase()) && w.chars().any(|c| c.is_lowercase()) {
        return Some(Why::CamelCase);
    }
    if !at_sentence_start && w.chars().next().is_some_and(|c| c.is_uppercase()) {
        return Some(Why::ProperNoun);
    }
    None
}

/// Pull candidates out of a piece of text.
pub fn from_text(text: &str, found: &mut HashMap<String, (Why, usize)>) {
    let mut start_of_sentence = true;
    for raw in text.split_whitespace() {
        let ends_sentence = raw.ends_with('.') || raw.ends_with('!') || raw.ends_with('?');
        if let Some(why) = classify(raw, start_of_sentence) {
            let term = trim_word(raw).to_string();
            let e = found.entry(term).or_insert((why, 0));
            e.1 += 1;
            // Stronger evidence wins: a word seen once as an identifier and
            // once capitalised is an identifier.
            if rank_of(why) > rank_of(e.0) {
                e.0 = why;
            }
        }
        // A line break also starts a sentence, but `split_whitespace` has
        // already eaten it; ending punctuation is what is left to go on.
        start_of_sentence = ends_sentence;
    }
}

/// How much a reason is worth when the same word is seen more than one way.
fn rank_of(w: Why) -> u8 {
    match w {
        Why::Identifier => 5,
        Why::Dotted => 4,
        Why::CamelCase => 3,
        Why::Acronym => 2,
        Why::ProperNoun => 1,
    }
}

/// Rank what was found: strongest evidence first, then most often seen, then
/// alphabetically so the list does not shuffle between runs.
pub fn rank(found: HashMap<String, (Why, usize)>, already: &[String], dismissed: &[String]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = found
        .into_iter()
        .filter(|(t, _)| !already.iter().any(|a| a.eq_ignore_ascii_case(t)))
        .filter(|(t, _)| !dismissed.iter().any(|d| d.eq_ignore_ascii_case(t)))
        .map(|(term, (why, count))| Candidate { term, why, count })
        .collect();
    out.sort_by(|a, b| {
        rank_of(b.why)
            .cmp(&rank_of(a.why))
            .then(b.count.cmp(&a.count))
            .then(a.term.to_lowercase().cmp(&b.term.to_lowercase()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(texts: &[&str]) -> HashMap<String, (Why, usize)> {
        let mut m = HashMap::new();
        for t in texts {
            from_text(t, &mut m);
        }
        m
    }

    #[test]
    fn identifiers_are_the_strongest_signal() {
        // The case whisper most reliably mangles: it hears "customer I D".
        assert_eq!(classify("customer_id", false), Some(Why::Identifier));
        assert_eq!(classify("fact_tickets", true), Some(Why::Identifier));
    }

    #[test]
    fn the_shapes_of_a_name_are_told_apart() {
        assert_eq!(classify("AccuLynx", false), Some(Why::CamelCase));
        assert_eq!(classify("MRR", false), Some(Why::Acronym));
        assert_eq!(classify("whisper.cpp", false), Some(Why::Dotted));
        assert_eq!(classify("Snowflake", false), Some(Why::ProperNoun));
    }

    #[test]
    fn a_capital_at_the_start_of_a_sentence_proves_nothing() {
        // Otherwise every sentence in every note becomes a candidate.
        assert_eq!(classify("Something", true), None);
        assert_eq!(classify("Something", false), Some(Why::ProperNoun));
        // ...but a shape that is distinctive on its own still counts there.
        assert_eq!(classify("AccuLynx", true), Some(Why::CamelCase));
    }

    #[test]
    fn ordinary_words_are_left_alone() {
        for w in ["the", "would", "think", "a", "an", "it"] {
            assert_eq!(classify(w, false), None, "{w} should not be a candidate");
        }
        // Capitalised common words too — "Their" opening a clause is not a name.
        assert_eq!(classify("Their", false), None);
    }

    #[test]
    fn punctuation_is_trimmed_but_inner_dots_survive() {
        assert_eq!(classify("Snowflake,", false), Some(Why::ProperNoun));
        assert_eq!(classify("(Pendo)", false), Some(Why::ProperNoun));
        // A sentence-ending dot goes; the one inside whisper.cpp stays.
        let mut m = HashMap::new();
        from_text("We use whisper.cpp. Pendo too.", &mut m);
        assert!(m.contains_key("whisper.cpp"), "got {:?}", m.keys().collect::<Vec<_>>());
    }

    #[test]
    fn a_word_seen_twice_ranks_above_one_seen_once() {
        let m = found(&["Pendo and Pendo again with Zendesk"]);
        let r = rank(m, &[], &[]);
        let pendo = r.iter().position(|c| c.term == "Pendo").unwrap();
        let zendesk = r.iter().position(|c| c.term == "Zendesk").unwrap();
        assert!(pendo < zendesk, "the more common word should come first");
    }

    #[test]
    fn stronger_evidence_outranks_being_common() {
        // An identifier seen once beats a proper noun seen five times: the
        // identifier is the one whisper will certainly get wrong.
        let m = found(&["Pendo Pendo Pendo Pendo Pendo and customer_id"]);
        let r = rank(m, &[], &[]);
        assert_eq!(r[0].term, "customer_id");
    }

    #[test]
    fn what_is_already_known_or_refused_is_not_offered_again() {
        let m = found(&["Pendo and Zendesk and customer_id"]);
        let r = rank(m, &["pendo".into()], &["Zendesk".into()]);
        let terms: Vec<_> = r.iter().map(|c| c.term.as_str()).collect();
        assert_eq!(terms, ["customer_id"]);
    }

    #[test]
    fn the_order_does_not_shuffle_between_runs() {
        // A list that reorders itself is a list nobody trusts. Every name is
        // mid-sentence here: the first word of a text starts one, so a name
        // in that position is not a candidate at all and the two sets would
        // differ for a reason that has nothing to do with ordering.
        let a = rank(found(&["we use Zendesk Pendo Looker"]), &[], &[]);
        let b = rank(found(&["we use Looker Pendo Zendesk"]), &[], &[]);
        let ta: Vec<_> = a.iter().map(|c| &c.term).collect();
        let tb: Vec<_> = b.iter().map(|c| &c.term).collect();
        assert_eq!(ta, tb);
        assert_eq!(ta.len(), 3);
    }

    #[test]
    fn a_name_opening_a_note_is_missed_and_that_is_accepted() {
        // The cost of not treating every sentence start as a name. It is the
        // right trade — the alternative offers "Something" and "Their" from
        // every note — and the word is picked up the moment it appears
        // anywhere else.
        let m = found(&["Pendo is the one."]);
        assert!(rank(m, &[], &[]).is_empty());
        let m = found(&["Pendo is the one. We also use Pendo."]);
        assert_eq!(rank(m, &[], &[])[0].term, "Pendo");
    }

    #[test]
    fn a_pasted_hash_is_not_a_name() {
        let long = "a".repeat(MAX_LEN + 1);
        assert_eq!(classify(&long, false), None);
        assert_eq!(classify("ab", false), None, "too short to be worth teaching");
    }
}
