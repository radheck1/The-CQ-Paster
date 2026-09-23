//! A dictation waiting for somewhere to land.
//!
//! If the keyboard is held by something that takes no text — a Finder window,
//! a button, a video — pasting would throw the words at whatever happened to
//! be in front. Instead the text goes on the clipboard and waits, and the next
//! click into something that does take text gets it.
//!
//! ## What this costs, stated plainly
//!
//! The clipboard is borrowed for as long as the wait lasts. CQ's chord paste
//! borrows it too, but for a fraction of a second; here it can be minutes, so
//! a Cmd+V in the meantime gives back the dictation rather than whatever was
//! copied before. That is the price of the text surviving at all, and the
//! original is put back the moment the dictation lands.
//!
//! ## And why it gives up
//!
//! Waiting is only useful if it ends. An application that never describes its
//! focus clearly would never satisfy the test, so the wait would last for ever
//! and the borrowed clipboard with it. After `PATIENCE` the dictation is left
//! on the clipboard — still reachable with Cmd+V, which is the least useless
//! thing to do with it — and the wait is over.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::clipboard::ClipSnapshot;

/// How long a dictation waits for somewhere to go.
///
/// Long enough to switch application, find the window and click into a field;
/// short enough that a forgotten dictation does not hold the clipboard all
/// afternoon.
pub const PATIENCE: Duration = Duration::from_secs(90);

/// How long to let the click settle before asking what has the keyboard.
///
/// The mouse-up arrives before the application has moved the focus, so asking
/// immediately reports whatever was focused *before* the click — which is the
/// thing that was not editable, and the wait would never end.
pub const SETTLE: Duration = Duration::from_millis(120);

struct Waiting {
    text: String,
    /// What was on the clipboard before, to be put back once the dictation
    /// lands. `None` when there was nothing worth restoring.
    borrowed: Option<ClipSnapshot>,
    since: Instant,
}

static WAITING: Mutex<Option<Waiting>> = Mutex::new(None);

/// Is a dictation waiting? Read by the keyboard tap on every click, so it is
/// one lock and nothing else.
pub fn is_waiting() -> bool {
    WAITING.lock().unwrap().is_some()
}

/// Begin waiting. Any dictation already waiting is replaced — the newer one is
/// what the user just said, and holding two would mean pasting the wrong one.
pub fn begin(text: String, borrowed: Option<ClipSnapshot>) {
    let mut w = WAITING.lock().unwrap();
    if let Some(old) = w.take() {
        crate::diag(&format!(
            "dictate: replacing a dictation that waited {:.0}s without landing",
            old.since.elapsed().as_secs_f32()
        ));
    }
    *w = Some(Waiting { text, borrowed, since: Instant::now() });
}

/// What to do now that a click has landed somewhere.
pub enum Next {
    /// Paste this, then put the clipboard back as it was.
    Paste(String, Option<ClipSnapshot>),
    /// Keep waiting.
    Hold,
    /// Waited long enough. The text stays on the clipboard.
    GaveUp,
}

/// Decide what a click means. Pure, so the rules are testable without a mouse.
pub fn decide(target: super::focus::Target, waited: Duration) -> Next {
    if waited >= PATIENCE {
        return Next::GaveUp;
    }
    match target {
        // Only a clear yes. An unclear answer is why the dictation is waiting
        // in the first place, so treating it as a yes here would paste into
        // exactly the place that could not be vouched for.
        super::focus::Target::Editable => Next::Paste(String::new(), None),
        _ => Next::Hold,
    }
}

/// A click happened. Returns what to paste, if anything.
pub fn on_click(target: super::focus::Target) -> Next {
    let mut guard = WAITING.lock().unwrap();
    let Some(w) = guard.as_ref() else {
        return Next::Hold;
    };
    match decide(target, w.since.elapsed()) {
        Next::Paste(..) => {
            let w = guard.take().unwrap();
            Next::Paste(w.text, w.borrowed)
        }
        Next::GaveUp => {
            let w = guard.take().unwrap();
            crate::diag(&format!(
                "dictate: nowhere to paste after {:.0}s — leaving it on the clipboard",
                w.since.elapsed().as_secs_f32()
            ));
            Next::GaveUp
        }
        Next::Hold => Next::Hold,
    }
}

/// Give up on whatever is waiting, without pasting. Used when dictation starts
/// again: the clipboard is about to be needed for the new one.
pub fn clear() {
    let _ = WAITING.lock().unwrap().take();
}

#[cfg(test)]
mod tests {
    use super::super::focus::Target;
    use super::*;

    #[test]
    fn a_clear_text_field_takes_the_paste() {
        assert!(matches!(decide(Target::Editable, Duration::ZERO), Next::Paste(..)));
    }

    #[test]
    fn anything_less_than_clear_keeps_waiting() {
        // The dictation is waiting *because* the focus could not be vouched
        // for; accepting an unclear answer here would paste into exactly that.
        assert!(matches!(decide(Target::Unknown, Duration::ZERO), Next::Hold));
        assert!(matches!(decide(Target::NotEditable, Duration::ZERO), Next::Hold));
    }

    #[test]
    fn it_gives_up_rather_than_waiting_for_ever() {
        // An application that never describes its focus would otherwise hold
        // the clipboard until CQ was quit.
        assert!(matches!(decide(Target::Unknown, PATIENCE), Next::GaveUp));
        assert!(matches!(decide(Target::NotEditable, PATIENCE + Duration::from_secs(1)), Next::GaveUp));
    }

    #[test]
    fn giving_up_beats_pasting_once_the_time_is_up() {
        // Finding a text field a hundred seconds later is not the click that
        // dictation was waiting for; by then the user has moved on.
        assert!(matches!(decide(Target::Editable, PATIENCE), Next::GaveUp));
    }

    #[test]
    fn the_settle_delay_is_long_enough_to_matter() {
        // Asking before the application has moved the focus reports what was
        // focused *before* the click — the thing that was not editable — and
        // the wait would never end.
        assert!(SETTLE >= Duration::from_millis(50));
        assert!(SETTLE < Duration::from_millis(400), "a visible lag after every click");
    }
}
