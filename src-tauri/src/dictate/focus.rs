//! Can whatever has the keyboard right now accept text?
//!
//! Dictation pastes where the cursor is, which is wrong when there is nowhere
//! for the text to go — a Finder window, a video, a button. Asking macOS what
//! holds the focus lets the text wait on the clipboard until it can land
//! somewhere instead of being thrown at whatever happened to be in front.
//!
//! This reads the accessibility tree, the same permission the keyboard tap
//! already needs, so it asks for nothing new.
//!
//! ## It is often wrong, and that is the point of `Unknown`
//!
//! Plenty of applications describe their focus badly or not at all — anything
//! drawing its own widgets, web views, games, older toolkits. There are three
//! answers rather than two so the caller can decide what an unclear one means,
//! and the decision is not buried here.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2_foundation::NSString;

type AXUIElementRef = *mut c_void;
type CFTypeRef = *mut c_void;
type CFStringRef = *const c_void;

const AX_SUCCESS: i32 = 0;

/// `kAXValueTypeCFRange`, the tag an `AXValue` carries when it wraps a range.
const AX_VALUE_CF_RANGE: u32 = 4;

/// A range of text, counted in UTF-16 units — which is what the accessibility
/// API counts in, and the reason [`before_index`] does the same.
#[repr(C)]
#[derive(Clone, Copy)]
struct CFRange {
    location: isize,
    length: isize,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        parameter: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
    fn AXValueCreate(the_type: u32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
}

/// What holds the keyboard, as far as it can be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A text field, a text area, something that takes typing.
    Editable,
    /// Clearly not: a button, a list, a window with nothing focused.
    NotEditable,
    /// Nothing useful came back. Common, and not a failure — many
    /// applications simply do not describe themselves.
    Unknown,
}

/// What sits immediately in front of the insertion point.
///
/// Three answers for the same reason [`Target`] has three: "the field begins
/// here" and "this application will not say" are different facts, and only the
/// caller knows what to make of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    /// The caret is at the very start, with nothing in front of it.
    Start,
    /// This character is immediately before the caret. If text is selected,
    /// it is the character before the selection — that is where the paste
    /// will begin, since the selection is about to be replaced.
    Char(char),
    /// The application did not say. Nothing is read into this: plenty of
    /// applications answer about their focus and nothing else.
    Unknown,
}

/// One look at whatever has the keyboard.
///
/// Both answers come from a single walk of the accessibility tree, because
/// they come from the same element and asking twice would be asking about two
/// different moments.
pub struct Look {
    pub target: Target,
    pub before: Before,
    /// The role the application gave, kept for the log alone.
    ///
    /// Nothing branches on it. It is here because which applications report
    /// what is the one thing that cannot be found out from outside CQ — the
    /// accessibility permission belongs to the app, not to a probe — and every
    /// question of that kind so far has been settled by logging it and looking
    /// a day later.
    pub role: Option<String>,
}

/// Roles that take typed text.
const EDITABLE: &[&str] = &[
    "AXTextField",
    "AXTextArea",
    "AXComboBox",
    "AXSearchField",
    "AXSecureTextField",
];

/// Roles that plainly do not, listed rather than assumed: everything else is
/// `Unknown`, because guessing "not editable" from an unrecognised role is how
/// dictation would stop working in an application that describes itself
/// unusually.
const NOT_EDITABLE: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXSlider",
    "AXMenuItem",
    "AXMenuBar",
    "AXMenuBarItem",
    "AXImage",
    "AXStaticText",
    "AXList",
    "AXTable",
    "AXOutline",
    "AXRow",
    "AXCell",
    "AXToolbar",
    "AXTabGroup",
    "AXWindow",
    "AXSheet",
    "AXPopUpButton",
    "AXDisclosureTriangle",
    "AXProgressIndicator",
];

/// Decide from what the accessibility tree said. Pure, so the rules can be
/// read and tested without a running application.
///
/// `settable` is whether the element's value can be written. It promotes an
/// unrecognised role to editable — a web view's custom field often reports a
/// role nobody has heard of but is honest about being writable — and it is
/// never used to demote, since plenty of controls that take no text have a
/// settable value.
pub fn classify(role: Option<&str>, settable: bool) -> Target {
    match role {
        None => Target::Unknown,
        Some(r) if EDITABLE.contains(&r) => Target::Editable,
        Some(r) if NOT_EDITABLE.contains(&r) => Target::NotEditable,
        Some(_) if settable => Target::Editable,
        Some(_) => Target::Unknown,
    }
}

fn cf(s: &Retained<NSString>) -> CFStringRef {
    // NSString and CFString are the same object.
    Retained::as_ptr(s).cast()
}

/// How much text CQ will copy out of a field to find one character of it.
///
/// Only reached when an application answers `AXValue` but not the read that
/// asks for a single character, and it runs between the last word spoken and
/// the paste, where a wait is felt. A field longer than this is left alone:
/// somebody dictating into a document of that size is not waiting on a space.
const TOO_BIG_TO_COPY: usize = 50_000;

/// The character before `caret`, where `caret` is a UTF-16 offset because that
/// is the unit the accessibility API counts in.
///
/// Pure, so the arithmetic — and the surrogate pair that makes it awkward —
/// can be tested without an application to ask.
pub fn before_index(s: &str, caret: usize) -> Before {
    if caret == 0 {
        return Before::Start;
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let at = caret.min(units.len());
    if at == 0 {
        return Before::Start;
    }
    // Two units, not one: a character outside the basic plane is stored as a
    // surrogate pair, and half of one decodes to nothing useful. Taking the
    // last of what decodes gets the whole character either way.
    let from = at.saturating_sub(2);
    String::from_utf16_lossy(&units[from..at])
        .chars()
        .next_back()
        .map(Before::Char)
        .unwrap_or(Before::Start)
}

/// Read one character out of an element by asking for that character alone.
///
/// The cheap path, and the one most applications support: it does not copy a
/// document to look at its last letter.
unsafe fn one_character_at(element: AXUIElementRef, at: isize) -> Option<char> {
    let range = CFRange { location: at, length: 1 };
    let param = AXValueCreate(AX_VALUE_CF_RANGE, (&range as *const CFRange).cast());
    if param.is_null() {
        return None;
    }
    let attr = NSString::from_str("AXStringForRange");
    let mut out: CFTypeRef = std::ptr::null_mut();
    let got = AXUIElementCopyParameterizedAttributeValue(element, cf(&attr), param, &mut out);
    CFRelease(param);
    if got != AX_SUCCESS || out.is_null() {
        return None;
    }
    if CFGetTypeID(out) != CFStringGetTypeID() {
        CFRelease(out);
        return None;
    }
    let s: &NSString = &*out.cast::<NSString>();
    let owned = s.to_string();
    CFRelease(out);
    owned.chars().next()
}

/// The fallback: copy the field's text and index it. Used only when the cheap
/// path is unsupported, which some applications' fields are.
unsafe fn from_whole_value(element: AXUIElementRef, caret: isize) -> Before {
    if caret as usize > TOO_BIG_TO_COPY {
        return Before::Unknown;
    }
    let attr = NSString::from_str("AXValue");
    let mut value: CFTypeRef = std::ptr::null_mut();
    if AXUIElementCopyAttributeValue(element, cf(&attr), &mut value) != AX_SUCCESS
        || value.is_null()
    {
        return Before::Unknown;
    }
    // A slider's AXValue is a number, not a string. Nothing here should reach
    // one — it got this far by having a text selection — but casting a number
    // to a string would be the kind of mistake that crashes rather than
    // misbehaves.
    if CFGetTypeID(value) != CFStringGetTypeID() {
        CFRelease(value);
        return Before::Unknown;
    }
    let s: &NSString = &*value.cast::<NSString>();
    let owned = s.to_string();
    CFRelease(value);
    before_index(&owned, caret as usize)
}

/// What is in front of the caret in `element`.
unsafe fn before_caret(element: AXUIElementRef) -> Before {
    let attr = NSString::from_str("AXSelectedTextRange");
    let mut value: CFTypeRef = std::ptr::null_mut();
    if AXUIElementCopyAttributeValue(element, cf(&attr), &mut value) != AX_SUCCESS
        || value.is_null()
    {
        return Before::Unknown;
    }
    let mut range = CFRange { location: 0, length: 0 };
    let read = AXValueGetValue(value, AX_VALUE_CF_RANGE, (&mut range as *mut CFRange).cast());
    CFRelease(value);
    if read == 0 {
        return Before::Unknown;
    }
    if range.location <= 0 {
        return Before::Start;
    }
    match one_character_at(element, range.location - 1) {
        Some(c) => Before::Char(c),
        None => from_whole_value(element, range.location),
    }
}

/// Ask macOS what has the keyboard, and what is written just before the caret.
pub fn look() -> Look {
    unsafe {
        let nothing =
            Look { target: Target::Unknown, before: Before::Unknown, role: None };
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return nothing;
        }
        let focused_attr = NSString::from_str("AXFocusedUIElement");
        let mut element: CFTypeRef = std::ptr::null_mut();
        let got = AXUIElementCopyAttributeValue(system, cf(&focused_attr), &mut element);
        CFRelease(system);
        if got != AX_SUCCESS || element.is_null() {
            // No focused element at all. That is a real answer — a Finder
            // window with nothing selected reports exactly this — but it is
            // also what a silent application looks like, so it is not taken
            // as proof either way.
            return nothing;
        }

        let role_attr = NSString::from_str("AXRole");
        let mut role_value: CFTypeRef = std::ptr::null_mut();
        let role = if AXUIElementCopyAttributeValue(element, cf(&role_attr), &mut role_value)
            == AX_SUCCESS
            && !role_value.is_null()
        {
            // AXRole is a CFString, which is an NSString.
            let s: &NSString = &*role_value.cast::<NSString>();
            let owned = s.to_string();
            CFRelease(role_value);
            Some(owned)
        } else {
            None
        };

        let value_attr = NSString::from_str("AXValue");
        let mut settable: u8 = 0;
        let _ = AXUIElementIsAttributeSettable(element, cf(&value_attr), &mut settable);

        let target = classify(role.as_deref(), settable != 0);
        // Only asked when the text is going somewhere. Anything else is about
        // to be parked on the clipboard, and where it eventually lands is a
        // different element entirely — asking this one would be asking about
        // the wrong document.
        let before = match target {
            Target::Editable => before_caret(element),
            _ => Before::Unknown,
        };

        CFRelease(element);
        Look { target, before, role }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usual_text_fields_are_recognised() {
        for r in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert_eq!(classify(Some(r), false), Target::Editable, "{r}");
        }
    }

    #[test]
    fn things_that_plainly_take_no_text_are_recognised() {
        for r in ["AXButton", "AXImage", "AXRow", "AXMenuItem", "AXWindow"] {
            assert_eq!(classify(Some(r), false), Target::NotEditable, "{r}");
        }
    }

    #[test]
    fn a_control_that_takes_no_text_is_not_promoted_by_being_settable() {
        // A slider and a checkbox both have a settable value, and neither is
        // anywhere to put a sentence. The known-role answer has to win.
        assert_eq!(classify(Some("AXSlider"), true), Target::NotEditable);
        assert_eq!(classify(Some("AXCheckBox"), true), Target::NotEditable);
    }

    #[test]
    fn an_unrecognised_role_that_admits_it_is_writable_is_taken_as_editable() {
        // Web views and custom toolkits invent roles; being honest about
        // AXValue is the best evidence available.
        assert_eq!(classify(Some("AXWebArea"), true), Target::Editable);
        assert_eq!(classify(Some("SomeCustomThing"), true), Target::Editable);
    }

    #[test]
    fn silence_is_unknown_not_a_no() {
        // The distinction the whole feature turns on: an application that says
        // nothing has not said "no".
        assert_eq!(classify(None, false), Target::Unknown);
        assert_eq!(classify(None, true), Target::Unknown);
        assert_eq!(classify(Some("AXGroup"), false), Target::Unknown);
    }

    #[test]
    fn the_character_before_the_caret_is_the_one_you_would_point_at() {
        let s = "Two things I need you to work on.";
        assert_eq!(before_index(s, s.len()), Before::Char('.'));
        assert_eq!(before_index(s, 3), Before::Char('o'));
        assert_eq!(before_index(s, 4), Before::Char(' '));
    }

    #[test]
    fn a_caret_at_the_start_has_nothing_in_front_of_it() {
        assert_eq!(before_index("hello", 0), Before::Start);
        assert_eq!(before_index("", 0), Before::Start);
        assert_eq!(before_index("", 5), Before::Start);
    }

    #[test]
    fn a_caret_past_the_end_reads_the_last_character_instead_of_panicking() {
        // An application can report a stale range. Clamping is the only
        // sensible reading, and indexing past the end would take the whole
        // paste down with it.
        assert_eq!(before_index("abc", 99), Before::Char('c'));
    }

    #[test]
    fn utf16_offsets_are_counted_the_way_the_accessibility_api_counts_them() {
        // "café" is 5 bytes and 4 UTF-16 units; an offset of 4 is the end.
        assert_eq!(before_index("café", 4), Before::Char('é'));
        // An emoji is one character over two UTF-16 units, so the caret after
        // it sits at 2 — and the character before it is the whole emoji, not
        // half of one.
        assert_eq!(before_index("🙂", 2), Before::Char('🙂'));
        assert_eq!(before_index("a🙂", 3), Before::Char('🙂'));
    }

    #[test]
    fn a_caret_split_inside_a_surrogate_pair_does_not_produce_a_character() {
        // Should not happen, but an application reporting a half-offset must
        // not be able to make this return nonsense that reads as a letter.
        assert!(matches!(before_index("🙂", 1), Before::Char('\u{fffd}')));
    }

    /// Not a unit test of logic — it asks the running system, and only checks
    /// that the call is safe and answers. Ignored by default because the
    /// answer depends on whatever has the keyboard when it runs.
    #[test]
    #[ignore = "asks the live system; the answer depends on what is focused"]
    fn asking_the_system_does_not_crash() {
        let l = look();
        assert!(matches!(
            l.target,
            Target::Editable | Target::NotEditable | Target::Unknown
        ));
        assert!(matches!(
            l.before,
            Before::Start | Before::Char(_) | Before::Unknown
        ));
    }
}
