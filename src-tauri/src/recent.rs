//! Which of CQ's tools did something most recently.
//!
//! Shake opens the control panel, and the question this answers is *on which
//! tool*. The user's own words: "CQ should open up where the most recent thing
//! occurred" — copy a slot and shake, and Paster should be in front; take a
//! screenshot and shake, and Shotter should be.
//!
//! ## It deliberately has no clock
//!
//! An earlier sketch held a timestamp per tool and ignored anything older than
//! an hour. That was offered and turned down in favour of "the most recent
//! thing wins, however old it is" — which removes the clock, the staleness
//! constant, and every question about what happens at the boundary. The last
//! write wins, and that is the whole rule.
//!
//! ## And no file
//!
//! Held in memory, so a restart of CQ forgets it. That is deliberate rather
//! than lazy: it gives the fallback something to do. With nothing recorded,
//! shake opens whichever tool the control panel was last showing — which the
//! frontend already remembers in `jotter.json`, and which is what shake did
//! before this existed. So the feature can only ever add: it changes what
//! happens after you have used a tool, and leaves a fresh start alone.
//!
//! ## Why Jotter reports in too
//!
//! Jotter has no hotkey of its own, so it would be tempting to leave it to the
//! remembered view. That breaks as soon as the two disagree: copy a slot, then
//! open Jotter by hand, and the copy would be the only thing recorded — so
//! shake would open Paster, over the top of the tool the user had just chosen.
//! Switching the view is using a tool.

use std::sync::Mutex;

/// One of CQ's three tools.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Paster,
    Shotter,
    Jotter,
}

impl Tool {
    /// The name the control panel knows this tool by — the same strings as
    /// `View` in `jotter.ts`, so the event needs no translating at the other
    /// end.
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Paster => "paster",
            Tool::Shotter => "shotter",
            Tool::Jotter => "jotter",
        }
    }

    /// Read back from the control panel, which is the only place these strings
    /// come from. An unknown one is not an error worth failing over — it means
    /// a view exists that this does not know about yet, and the right answer
    /// then is to record nothing and let the remembered view stand.
    pub fn from_str(s: &str) -> Option<Tool> {
        match s {
            "paster" => Some(Tool::Paster),
            "shotter" => Some(Tool::Shotter),
            "jotter" => Some(Tool::Jotter),
            _ => None,
        }
    }
}

static LAST: Mutex<Option<Tool>> = Mutex::new(None);

/// Note that a tool was used. Called from the hook's worker thread, so it is
/// one lock and nothing else — never from the tap callback.
pub fn used(tool: Tool) {
    *LAST.lock().unwrap() = Some(tool);
}

/// Which tool was used last, if any has been.
pub fn last() -> Option<Tool> {
    *LAST.lock().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One global, so these run in one test rather than racing each other.
    #[test]
    fn the_most_recent_use_is_the_one_remembered() {
        assert_eq!(last(), None, "nothing used yet");

        used(Tool::Paster);
        assert_eq!(last(), Some(Tool::Paster));

        // Later wins, with no notion of how much later.
        used(Tool::Shotter);
        assert_eq!(last(), Some(Tool::Shotter));

        // Including going back to one already seen.
        used(Tool::Paster);
        assert_eq!(last(), Some(Tool::Paster));

        // The case the Jotter writer exists for: a slot copy followed by the
        // user opening Jotter by hand must leave Jotter in front.
        used(Tool::Jotter);
        assert_eq!(last(), Some(Tool::Jotter));
    }

    #[test]
    fn the_names_match_the_control_panel_and_survive_a_round_trip() {
        for t in [Tool::Paster, Tool::Shotter, Tool::Jotter] {
            assert_eq!(Tool::from_str(t.as_str()), Some(t), "{t:?}");
        }
        // Exactly the strings `View` uses in jotter.ts. If that type gains a
        // member, this is where it has to be taught about it.
        assert_eq!(Tool::Paster.as_str(), "paster");
        assert_eq!(Tool::Shotter.as_str(), "shotter");
        assert_eq!(Tool::Jotter.as_str(), "jotter");
    }

    #[test]
    fn a_view_this_does_not_know_about_records_nothing() {
        assert_eq!(Tool::from_str("meeter"), None);
        assert_eq!(Tool::from_str(""), None);
        assert_eq!(Tool::from_str("Paster"), None, "the panel sends lowercase");
    }
}
