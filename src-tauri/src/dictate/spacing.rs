//! Joining a dictation to what is already written.
//!
//! Dictating a sentence at a time into one box is the normal way to use this,
//! and whisper hands back a sentence with no space on either end. So the
//! second sentence arrives welded to the first:
//!
//! ```text
//! Two things I need you to work on.The other thing is, because I use this…
//! ```
//!
//! The fix is a space in front of what is pasted — not an edit to what is
//! already there. Nothing the user has written is touched, which matters: the
//! text in front of the caret belongs to them, and reaching into another
//! application's document to add a character to it is a far larger thing to do
//! than adding one to our own.
//!
//! ## Which way the mistake falls
//!
//! There is no answer that is right every time, because the space depends on a
//! character CQ can only ask about. When the application will not say (see
//! [`Before::Unknown`]) nothing is added, which is what CQ did before this
//! existed — so an application that stays silent keeps exactly the behaviour
//! it had, and the change can only ever help.

use super::focus::Before;

/// Marks that belong to the word in front of them. A dictation starting with
/// one of these wants no space — it would be left stranded.
///
/// Rare, since whisper does not usually start a sentence with a comma, but
/// cheap to be right about.
const ATTACHES_LEFT: &[char] = &[
    ',', '.', '!', '?', ';', ':', ')', ']', '}', '%', '…', '\'', '’', '”',
];

/// Characters the next word attaches to, so nothing goes between them.
///
/// The openers are the clear cases: type `(` and speak, and the word belongs
/// against the bracket. `-`, `/` and `_` are the middles of words and paths.
/// `"` is here as a judgement call rather than a certainty — it is ambiguous,
/// since it both opens and closes, and someone who has just typed one and
/// started speaking is opening it.
const ATTACHES_RIGHT: &[char] = &[
    '(', '[', '{', '<', '"', '“', '‘', '«', '/', '\\', '-', '–', '—', '_', '@', '#', '$', '~',
];

/// Does `text` need a space in front of it to sit beside what is already
/// there? Pure, so every case below is decided without an application to ask.
pub fn needs_space(before: Before, text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    if first.is_whitespace() || ATTACHES_LEFT.contains(&first) {
        return false;
    }
    match before {
        // The caret is at the start of the field, so there is nothing to sit
        // beside.
        Before::Start => false,
        // Silence is not a no, but it is not a yes either, and this is the
        // side to be wrong on: doing nothing is what happened before.
        Before::Unknown => false,
        Before::Char(c) => !c.is_whitespace() && !ATTACHES_RIGHT.contains(&c),
    }
}

/// How the decision reads in the log, so a paste that came out wrong can be
/// looked into rather than guessed at.
///
/// A letter or a digit is named but not quoted. The diagnostic value is which
/// branch was taken, and the character itself is a piece of the user's
/// document — a password field is an editable field like any other, and one
/// character of one has no business in a log file. Punctuation is quoted,
/// since that is where the rules are arguable and there is nothing to give
/// away.
pub fn describe(before: Before) -> String {
    match before {
        Before::Start => "the caret is at the start of the field".into(),
        Before::Unknown => "the app would not say what is before the caret".into(),
        Before::Char(c) if c == '\n' || c == '\r' => "the caret is on a new line".into(),
        Before::Char(c) if c.is_whitespace() => "there is already a space before the caret".into(),
        Before::Char(c) if c.is_alphanumeric() => "the caret sits after a word".into(),
        Before::Char(c) => format!("the caret sits after {c:?}"),
    }
}

/// What goes between what is already written and what is being pasted, if
/// anything.
///
/// A list gets a line break rather than a space. Its first item would
/// otherwise be glued to the end of the previous sentence while every other
/// item sat on its own line, which is worse than not formatting it at all.
pub fn separator(before: Before, text: &str) -> Option<char> {
    if !needs_space(before, text) {
        return None;
    }
    Some(if text.contains('\n') { '\n' } else { ' ' })
}

/// The text to paste, with a space in front if one is missing.
///
/// Pure, and separate from [`lead`] so the tests can check what comes out
/// without writing to the diagnostics log — which is a real file in the user's
/// data folder, and a test suite has no business appending to it.
pub fn join(before: Before, text: String) -> String {
    match separator(before, &text) {
        Some(c) => format!("{c}{text}"),
        None => text,
    }
}

/// The text to paste. Says in the log what it saw and what it did with it.
pub fn lead(before: Before, text: String) -> String {
    crate::diag(&format!(
        "dictate: {} — {}",
        describe(before),
        match separator(before, &text) {
            Some('\n') => "starting it on a new line",
            Some(_) => "adding a space in front",
            None => "pasting as is",
        }
    ));
    join(before, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_after_a_sentence_gets_a_space() {
        // The case the whole module exists for, taken from a real pair of
        // dictations: two sentences spoken separately into one message box.
        assert!(needs_space(Before::Char('.'), "The other thing is, because I use this"));
        assert_eq!(
            join(Before::Char('.'), "The other thing is".into()),
            " The other thing is"
        );
    }

    #[test]
    fn a_space_already_there_is_not_doubled() {
        for c in [' ', '\t', '\n', '\u{a0}'] {
            assert!(!needs_space(Before::Char(c), "hello"), "{c:?}");
        }
    }

    #[test]
    fn the_start_of_an_empty_field_gets_nothing() {
        assert!(!needs_space(Before::Start, "hello"));
        assert_eq!(join(Before::Start, "hello".into()), "hello");
    }

    #[test]
    fn an_application_that_will_not_say_keeps_the_old_behaviour() {
        // The guarantee this module makes: silence changes nothing. Whatever
        // CQ pasted before, it pastes now.
        let text = "See the attached document.".to_string();
        assert_eq!(join(Before::Unknown, text.clone()), text);
    }

    #[test]
    fn a_word_gets_a_space_too_not_just_punctuation() {
        // Dictating mid-sentence after a word, which is just as common as
        // after a full stop.
        assert!(needs_space(Before::Char('g'), "and then some more"));
        assert!(needs_space(Before::Char('5'), "dollars"));
    }

    #[test]
    fn an_opener_keeps_the_word_against_it() {
        for c in ['(', '[', '{', '"', '“', '/', '-', '@', '_'] {
            assert!(!needs_space(Before::Char(c), "hello"), "{c:?}");
        }
    }

    #[test]
    fn a_closing_mark_is_not_an_opener() {
        // The other half of a quoted sentence: "…works." then a new sentence.
        for c in [')', ']', '”', '’', '!', '?'] {
            assert!(needs_space(Before::Char(c), "hello"), "{c:?}");
        }
    }

    #[test]
    fn a_dictation_starting_with_a_mark_is_not_stranded() {
        for t in [", and then", ". Next", "? Really", "'s mine", "… and so on"] {
            assert!(!needs_space(Before::Char('d'), t), "{t:?}");
        }
    }

    #[test]
    fn nothing_to_paste_needs_nothing_in_front_of_it() {
        assert!(!needs_space(Before::Char('.'), ""));
        assert_eq!(join(Before::Char('.'), String::new()), "");
    }

    #[test]
    fn text_that_already_leads_with_a_space_is_left_alone() {
        assert!(!needs_space(Before::Char('.'), " already spaced"));
    }

    #[test]
    fn a_list_starts_on_its_own_line_not_glued_to_the_sentence_before() {
        // Otherwise the first item trails the previous sentence while every
        // other item sits on its own line.
        let list = "eggs\nbacon\nmilk";
        assert_eq!(separator(Before::Char('.'), list), Some('\n'));
        assert_eq!(join(Before::Char('.'), list.into()), "\neggs\nbacon\nmilk");
        // One line still gets a space.
        assert_eq!(separator(Before::Char('.'), "eggs"), Some(' '));
    }

    #[test]
    fn a_list_at_the_start_of_a_field_gets_no_break_either() {
        assert_eq!(separator(Before::Start, "eggs\nbacon"), None);
        assert_eq!(separator(Before::Char('\n'), "eggs\nbacon"), None);
    }

    #[test]
    fn the_log_line_names_what_it_saw() {
        // The log is the only way to find out why a paste came out wrong, so
        // each answer has to be distinguishable in it.
        assert!(describe(Before::Unknown).contains("would not say"));
        assert!(describe(Before::Start).contains("start"));
        assert!(describe(Before::Char('\n')).contains("new line"));
        assert!(describe(Before::Char(' ')).contains("already a space"));
        assert!(describe(Before::Char('.')).contains("'.'"));
    }

    #[test]
    fn the_log_does_not_quote_a_character_of_the_users_text() {
        // A password field is an editable field, and the log is a file on
        // disk. Which branch was taken is the useful part; the letter is not.
        for c in ['a', 'Z', '7', 'é'] {
            let said = describe(Before::Char(c));
            assert!(said.contains("after a word"), "{c:?} -> {said}");
            // Not a bare substring check: "the caret sits after a word"
            // contains an 'a' of its own. What must not appear is the
            // character quoted, which is how `describe` reports punctuation.
            assert!(!said.contains(&format!("{c:?}")), "{c:?} leaked into {said}");
        }
    }
}
