//! macOS clipboard layer — `NSPasteboard` snapshot / restore.
//!
//! # Why the shape differs from Windows
//!
//! A Windows clipboard is a flat map of format-id -> bytes, which is what
//! [`super::ClipFormat`] models. A macOS pasteboard is a **list of items**, each
//! with its own set of UTIs. Finder represents a three-file copy as three items
//! carrying `public.file-url` — there is no single-blob equivalent of `CF_HDROP`
//! and no `NSFilenamesPboardType` alongside it on current macOS.
//!
//! Flattening items into one list is not a lossless simplification: setting the
//! same UTI twice on one pasteboard keeps only the first value **and still
//! reports success**, so a three-file copy silently pastes as one file. That is
//! the same class of failure as the Windows `raw::set` bug in §6.4, so the
//! macOS snapshot keeps items as the outer dimension.
//!
//! This changes the persisted `bincode` layout, which is safe here and only
//! here: macOS has no existing users. The Windows representation is untouched.

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use serde::{Deserialize, Serialize};

use super::{trim_preview, SlotPreview};

pub const UTI_TEXT: &str = "public.utf8-plain-text";
pub const UTI_TEXT16: &str = "public.utf16-external-plain-text";
// Named for the record: these are the rich-text types `text_only` deliberately
// drops. Nothing reads them, because it works from a keep-list of text types
// rather than a drop-list — a new rich type must not silently survive.
#[allow(dead_code)]
pub const UTI_HTML: &str = "public.html";
#[allow(dead_code)]
pub const UTI_RTF: &str = "public.rtf";
pub const UTI_PNG: &str = "public.png";
pub const UTI_TIFF: &str = "public.tiff";
pub const UTI_FILE_URL: &str = "public.file-url";

/// The convention password managers use to mark "do not record this".
///
/// Necessary but **not sufficient** — see [`is_sensitive`].
pub const UTI_CONCEALED: &str = "org.nspasteboard.ConcealedType";

/// One pasteboard type within an item: its UTI and raw bytes.
///
/// macOS identifies types by string UTI, not the numeric id Windows uses.
#[derive(Clone, Serialize, Deserialize)]
pub struct ClipType {
    pub uti: String,
    pub data: Vec<u8>,
}

/// One pasteboard item. Type order is preserved: the first type an item
/// declares is the one consumers prefer, so a round trip must not reorder them.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ClipItem {
    pub types: Vec<ClipType>,
}

impl ClipItem {
    fn find(&self, uti: &str) -> Option<&[u8]> {
        self.types
            .iter()
            .find(|t| t.uti == uti)
            .map(|t| t.data.as_slice())
    }
}

/// A full snapshot of the pasteboard: every item, every type, verbatim bytes.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ClipSnapshot {
    pub items: Vec<ClipItem>,
}

impl ClipSnapshot {
    /// True when the snapshot carries no actual bytes.
    ///
    /// Checks payloads, not just type names. A pasteboard caught mid-write has
    /// its types already declared but not yet filled, so counting declared
    /// types would call that a valid capture and store an empty slot.
    pub fn is_empty(&self) -> bool {
        !self
            .items
            .iter()
            .any(|i| i.types.iter().any(|t| !t.data.is_empty()))
    }

    /// True when the capture looks finished.
    ///
    /// A publisher declares its types and then fills them, so a type present
    /// with an empty payload means the copy is still in flight. WebKit is the
    /// clear case: it lands `com.apple.webarchive` first and leaves
    /// `public.html` and `public.utf8-plain-text` at 0 bytes for a moment.
    /// Accepting that capture stores a slot that previews as empty and pastes
    /// nothing, so "some type has bytes" is too weak a test — every declared
    /// type must have them.
    pub fn is_complete(&self) -> bool {
        !self.is_empty()
            && self
                .items
                .iter()
                .all(|i| i.types.iter().all(|t| !t.data.is_empty()))
    }

    /// The same snapshot without any declared-but-empty types.
    ///
    /// Used only as a fallback when a publisher never fills a type it declared:
    /// better to keep what did arrive than to store empty payloads that make
    /// previews blank and paste nothing.
    pub fn without_empty_types(&self) -> ClipSnapshot {
        ClipSnapshot {
            items: self
                .items
                .iter()
                .map(|i| ClipItem {
                    types: i.types.iter().filter(|t| !t.data.is_empty()).cloned().collect(),
                })
                .filter(|i: &ClipItem| !i.types.is_empty())
                .collect(),
        }
    }

    /// First occurrence of `uti` across all items, ignoring empty payloads so a
    /// declared-but-unfilled type never masks a real one.
    fn find(&self, uti: &str) -> Option<&[u8]> {
        self.items
            .iter()
            .find_map(|i| i.find(uti).filter(|d| !d.is_empty()))
    }

    fn has(&self, uti: &str) -> bool {
        self.find(uti).is_some()
    }
}

/// Types that must not be written back.
///
/// The macOS analogue of `is_ole_cookie`: handles that are only meaningful to
/// the process that published them. Chromium's render-frame token identifies a
/// frame that is long gone by the time a slot is pasted.
fn is_transient(uti: &str) -> bool {
    uti == "org.chromium.internal.source-rfh-token"
}

fn pasteboard() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

/// Resolve a `file-url` payload to a concrete path URL.
///
/// Finder publishes volume-id references (`file:///.file/id=6571367.46089685`)
/// rather than paths. Those are only valid while the file stays where it is,
/// and slots persist across restarts — so they are resolved at capture time,
/// mirroring the PIDL->path conversion `augment_files` does on Windows.
/// Returns `None` when the reference cannot be resolved (deleted file, unmounted
/// volume), in which case the original bytes are kept untouched.
fn resolved_file_url(raw: &[u8]) -> Option<Vec<u8>> {
    let url = std::str::from_utf8(raw).ok()?;
    if !url.contains("/.file/id=") {
        return None; // already a plain path URL
    }
    autoreleasepool(|_| {
        let s = NSString::from_str(url);
        let u = NSURL::URLWithString(&s)?;
        let p = u.filePathURL()?;
        let abs = p.absoluteString()?;
        Some(abs.to_string().into_bytes())
    })
}

/// Path (not URL) for a `public.file-url` payload, for previews.
fn path_of_file_url(raw: &[u8]) -> Option<String> {
    let url = std::str::from_utf8(raw).ok()?;
    autoreleasepool(|_| {
        let s = NSString::from_str(url);
        let u = NSURL::URLWithString(&s)?;
        let p = u.filePathURL().unwrap_or(u);
        p.path().map(|p| p.to_string())
    })
}

/// The pasteboard's change counter, which increments on every write by any
/// process. Lets a caller wait for a copy to actually land instead of guessing
/// at a duration.
pub fn change_count() -> i64 {
    autoreleasepool(|_| pasteboard().changeCount() as i64)
}

/// Capture every item and type currently on the pasteboard.
///
/// Types whose provider returns `nil` are lazy promises that never
/// materialised; they are dropped rather than stored as empty formats, since
/// writing an empty payload back would advertise a type we cannot supply.
pub fn snapshot() -> Result<ClipSnapshot, String> {
    autoreleasepool(|_| {
        let pb = pasteboard();
        let Some(items) = pb.pasteboardItems() else {
            return Ok(ClipSnapshot::default());
        };
        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for item in items.iter() {
            let mut types = Vec::new();
            for ty in item.types().iter() {
                let uti = ty.to_string();
                let mut data = item
                    .dataForType(&ty)
                    .map(|d| d.to_vec())
                    .unwrap_or_default();

                // Promised ("lazy") data: the per-item read hands back an empty
                // NSData, while the pasteboard-level read makes the owning app
                // actually produce the bytes. WebKit apps publish their text and
                // HTML this way, so without this a copy from one of them stores
                // only a webarchive and pastes nothing.
                //
                // Restricted to the first item declaring a given UTI, because
                // the pasteboard-level call always answers from that item — using
                // it for later items would copy item 0's payload across a
                // multi-item pasteboard such as a Finder file selection.
                let first_for_uti = !seen.contains(&uti);
                if data.is_empty() && first_for_uti {
                    if let Some(d) = pb.dataForType(&ty) {
                        data = d.to_vec();
                    }
                }
                seen.push(uti.clone());

                // Types left empty are deliberately kept: `is_complete` uses
                // them to tell a still-arriving copy from a finished one.
                if uti == UTI_FILE_URL && !data.is_empty() {
                    if let Some(resolved) = resolved_file_url(&data) {
                        data = resolved;
                    }
                }
                types.push(ClipType { uti, data });
            }
            out.push(ClipItem { types });
        }
        Ok(ClipSnapshot { items: out })
    })
}

/// Write a snapshot back to the pasteboard.
///
/// `clearContents` is called **exactly once** and every item is then written in
/// a single `writeObjects` — the macOS half of §6.4. Rebuilding items rather
/// than calling `setData:forType:` per type is what preserves multi-file copies.
pub fn restore(snap: &ClipSnapshot) -> Result<(), String> {
    autoreleasepool(|_| {
        let pb = pasteboard();
        pb.clearContents();

        let mut objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
        for item in &snap.items {
            let obj = NSPasteboardItem::new();
            let mut wrote_any = false;
            for t in &item.types {
                if is_transient(&t.uti) || t.data.is_empty() {
                    continue;
                }
                let ty = NSString::from_str(&t.uti);
                let data = NSData::with_bytes(&t.data);
                if obj.setData_forType(&data, &ty) {
                    wrote_any = true;
                }
            }
            if wrote_any {
                objs.push(ProtocolObject::from_retained(obj));
            }
        }
        if objs.is_empty() {
            return Err("nothing to restore".into());
        }
        let array = NSArray::from_retained_slice(&objs);
        if pb.writeObjects(&array) {
            Ok(())
        } else {
            Err("writeObjects failed".into())
        }
    })
}

/// True if the pasteboard owner marked its content as not-for-history.
///
/// # Known limitation
///
/// `org.nspasteboard.ConcealedType` is the only convention macOS offers, and it
/// is advisory: an app that does not set it is indistinguishable from any other
/// source. Diagnostics during the port confirmed a real password copied through
/// 1Password's **web** interface arrives with no marker at all — only
/// `public.utf8-plain-text` and Chromium's source types — and so **will** be
/// captured into a slot.
///
/// Windows has firmer ground here (`ExcludeClipboardContentFromMonitorProcessing`
/// and friends), so this is genuinely weaker on macOS rather than an oversight.
/// Anything stronger — a source-application denylist, say — is a product
/// decision, not something this function can infer.
pub fn is_sensitive() -> bool {
    autoreleasepool(|_| {
        let pb = pasteboard();
        let Some(items) = pb.pasteboardItems() else {
            return false;
        };
        items.iter().any(|item| {
            item.types()
                .iter()
                .any(|t| t.to_string() == UTI_CONCEALED)
        })
    })
}

/// No-op on macOS. Windows needs COM initialised per worker thread for shell
/// path resolution; `NSURL` has no equivalent requirement.
pub fn init_thread() {}

/// Strip everything but plain text, for "paste without formatting".
///
/// Returns a single item holding only the text types, so the target receives no
/// HTML or RTF to prefer. Chrome publishes `public.html` with no `public.rtf`,
/// so dropping HTML is what actually does the work here. Returns `None` when the
/// slot carries no text at all (an image or a pure file list).
pub fn text_only(snap: &ClipSnapshot) -> Option<ClipSnapshot> {
    let keep = [UTI_TEXT, UTI_TEXT16];
    // Empty payloads are skipped: a declared-but-unfilled text type would
    // otherwise pass as text and make a plain-text paste insert nothing.
    let types: Vec<ClipType> = snap
        .items
        .iter()
        .flat_map(|i| i.types.iter())
        .filter(|t| keep.contains(&t.uti.as_str()) && !t.data.is_empty())
        .cloned()
        .collect();
    if types.is_empty() {
        return None;
    }
    // Deduplicate: a UTI may only appear once per item.
    let mut seen = Vec::new();
    let types: Vec<ClipType> = types
        .into_iter()
        .filter(|t| {
            if seen.contains(&t.uti) {
                false
            } else {
                seen.push(t.uti.clone());
                true
            }
        })
        .collect();
    Some(ClipSnapshot {
        items: vec![ClipItem { types }],
    })
}

/// Decode `public.utf16-external-plain-text`, honouring the leading BOM.
fn utf16_external_to_string(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }
    let big_endian = data[0] == 0xFE && data[1] == 0xFF;
    let has_bom = big_endian || (data[0] == 0xFF && data[1] == 0xFE);
    let body = if has_bom { &data[2..] } else { data };
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Width/height from a PNG IHDR chunk.
fn png_dimensions(data: &[u8]) -> (Option<i32>, Option<i32>) {
    // 8-byte signature, 4-byte length, 4-byte "IHDR", then width and height.
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return (None, None);
    }
    let w = i32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = i32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    (Some(w), Some(h))
}

/// Build the popup/control-panel preview for a snapshot.
pub fn preview(snap: &ClipSnapshot) -> SlotPreview {
    let bytes: usize = snap
        .items
        .iter()
        .flat_map(|i| i.types.iter())
        .map(|t| t.data.len())
        .sum();

    // Files first: a file copy also carries text naming the files, and the file
    // list is the more useful answer. Every item contributes one URL.
    let files: Vec<String> = snap
        .items
        .iter()
        .filter_map(|i| i.find(UTI_FILE_URL))
        .filter_map(path_of_file_url)
        .collect();
    if !files.is_empty() {
        return SlotPreview {
            kind: "files".into(),
            text: None,
            files,
            bytes,
            width: None,
            height: None,
        };
    }

    if let Some(png) = snap.find(UTI_PNG) {
        let (width, height) = png_dimensions(png);
        return SlotPreview {
            kind: "image".into(),
            text: None,
            files: Vec::new(),
            bytes,
            width,
            height,
        };
    }
    if snap.has(UTI_TIFF) {
        // TIFF headers are far more involved to parse than PNG and the size is
        // only cosmetic, so it is left unreported rather than guessed at.
        return SlotPreview {
            kind: "image".into(),
            text: None,
            files: Vec::new(),
            bytes,
            width: None,
            height: None,
        };
    }

    if let Some(data) = snap.find(UTI_TEXT) {
        return SlotPreview {
            kind: "text".into(),
            text: Some(trim_preview(&String::from_utf8_lossy(data))),
            files: Vec::new(),
            bytes,
            width: None,
            height: None,
        };
    }
    if let Some(data) = snap.find(UTI_TEXT16) {
        return SlotPreview {
            kind: "text".into(),
            text: Some(trim_preview(&utf16_external_to_string(data))),
            files: Vec::new(),
            bytes,
            width: None,
            height: None,
        };
    }

    SlotPreview {
        kind: "other".into(),
        text: None,
        files: Vec::new(),
        bytes,
        width: None,
        height: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(pairs: &[(&str, &[u8])]) -> ClipItem {
        ClipItem {
            types: pairs
                .iter()
                .map(|(uti, data)| ClipType {
                    uti: (*uti).into(),
                    data: data.to_vec(),
                })
                .collect(),
        }
    }

    /// A three-file Finder copy is three items; the preview must list all three,
    /// not just the first. This is the regression the flat model would cause.
    #[test]
    fn preview_lists_every_file_in_a_multi_item_copy() {
        let snap = ClipSnapshot {
            items: vec![
                item(&[(UTI_FILE_URL, b"file:///tmp/a.txt")]),
                item(&[(UTI_FILE_URL, b"file:///tmp/b.txt")]),
                item(&[(UTI_FILE_URL, b"file:///tmp/c.txt")]),
            ],
        };
        let p = preview(&snap);
        assert_eq!(p.kind, "files");
        assert_eq!(p.files.len(), 3);
        assert!(p.files[2].ends_with("c.txt"), "got {:?}", p.files);
    }

    /// Finder puts the aggregated filename text on item 0 only, so a file copy
    /// must still be previewed as files rather than as that text.
    #[test]
    fn files_win_over_the_text_finder_attaches_to_item_zero() {
        let snap = ClipSnapshot {
            items: vec![
                item(&[
                    (UTI_FILE_URL, b"file:///tmp/a.txt"),
                    (UTI_TEXT, b"a.txt\rb.txt"),
                ]),
                item(&[(UTI_FILE_URL, b"file:///tmp/b.txt")]),
            ],
        };
        assert_eq!(preview(&snap).kind, "files");
    }

    /// The real shape WebKit publishes mid-copy: its own types filled, the
    /// content types declared and still empty. Accepting this stores a slot
    /// that previews blank and pastes nothing.
    #[test]
    fn a_half_written_webkit_pasteboard_is_not_complete() {
        let snap = ClipSnapshot {
            items: vec![item(&[
                ("com.apple.webarchive", &[0u8; 253]),
                (UTI_HTML, b""),
                (UTI_TEXT, b""),
                ("com.apple.WebKit.custom-pasteboard-data", &[0u8; 46]),
            ])],
        };
        assert!(!snap.is_empty(), "it does carry some bytes");
        assert!(!snap.is_complete(), "but the content types are unfilled");
        // A blank text type must not mask the absence of real text.
        assert_eq!(preview(&snap).text, None);
        assert!(text_only(&snap).is_none());
    }

    #[test]
    fn a_fully_written_pasteboard_is_complete() {
        let snap = ClipSnapshot {
            items: vec![item(&[(UTI_HTML, b"<b>hi</b>"), (UTI_TEXT, b"hi")])],
        };
        assert!(snap.is_complete());
    }

    #[test]
    fn without_empty_types_keeps_only_what_arrived() {
        let snap = ClipSnapshot {
            items: vec![
                item(&[("com.apple.webarchive", b"data"), (UTI_TEXT, b"")]),
                item(&[(UTI_HTML, b"")]),
            ],
        };
        let trimmed = snap.without_empty_types();
        assert_eq!(trimmed.items.len(), 1, "the all-empty item is dropped");
        assert_eq!(trimmed.items[0].types.len(), 1);
        assert_eq!(trimmed.items[0].types[0].uti, "com.apple.webarchive");
    }

    #[test]
    fn text_only_drops_html_and_keeps_text() {
        let snap = ClipSnapshot {
            items: vec![item(&[
                (UTI_HTML, b"<b>bold</b>"),
                (UTI_TEXT, b"bold"),
                ("org.chromium.source-url", b"https://example.com"),
            ])],
        };
        let stripped = text_only(&snap).expect("has text");
        let utis: Vec<&str> = stripped.items[0]
            .types
            .iter()
            .map(|t| t.uti.as_str())
            .collect();
        assert_eq!(utis, vec![UTI_TEXT]);
    }

    #[test]
    fn text_only_returns_none_for_an_image() {
        let snap = ClipSnapshot {
            items: vec![item(&[(UTI_PNG, b"\x89PNG\r\n\x1a\n")])],
        };
        assert!(text_only(&snap).is_none());
    }

    #[test]
    fn png_dimensions_read_the_ihdr() {
        let mut png = Vec::from(*b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1276u32.to_be_bytes());
        png.extend_from_slice(&418u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), (Some(1276), Some(418)));
    }

    #[test]
    fn png_dimensions_reject_non_png() {
        assert_eq!(png_dimensions(b"not a png at all......."), (None, None));
    }

    #[test]
    fn utf16_external_honours_the_bom() {
        // "hi" little-endian with BOM, as Finder publishes it.
        let le = [0xFF, 0xFE, b'h', 0x00, b'i', 0x00];
        assert_eq!(utf16_external_to_string(&le), "hi");
        let be = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i'];
        assert_eq!(utf16_external_to_string(&be), "hi");
    }

    /// The live tests below drive the one real, process-global pasteboard, and
    /// `cargo test` runs tests on separate threads by default — so without this
    /// they interleave and assert against each other's writes.
    static LIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Round trip through the *real* pasteboard: the three-file case that the
    /// flat model silently collapses to one.
    ///
    /// Ignored by default because it overwrites whatever the user has copied.
    /// Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "overwrites the real clipboard"]
    fn live_round_trip_preserves_every_item() {
        // Poisoning is irrelevant here: the lock guards ordering, not data.
        let _guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let original = ClipSnapshot {
            items: vec![
                item(&[(UTI_FILE_URL, b"file:///private/tmp/cqtest/a.txt")]),
                item(&[(UTI_FILE_URL, b"file:///private/tmp/cqtest/b.txt")]),
                item(&[(UTI_FILE_URL, b"file:///private/tmp/cqtest/c.txt")]),
            ],
        };
        restore(&original).expect("restore");
        let read_back = snapshot().expect("snapshot");
        assert_eq!(read_back.items.len(), 3, "items must survive the round trip");
        assert_eq!(preview(&read_back).files.len(), 3);
    }

    /// Reading a multi-item file pasteboard that something else wrote.
    ///
    /// The sibling round-trip test writes with our own `restore`, so it cannot
    /// catch a shared misunderstanding of the API. This one reads a pasteboard
    /// laid down by the standalone diagnostic instead:
    ///
    /// ```text
    /// pbdiag synth-files /tmp/cqtest/a.txt /tmp/cqtest/b.txt /tmp/cqtest/c.txt
    /// ```
    #[test]
    #[ignore = "needs pbdiag synth-files to have written the 3-file fixture"]
    fn live_reads_a_multi_item_file_pasteboard() {
        let _guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());

        let snap = snapshot().expect("snapshot");
        assert_eq!(snap.items.len(), 3, "one item per file");
        assert!(snap.is_complete());

        let p = preview(&snap);
        assert_eq!(p.kind, "files");
        assert_eq!(p.files.len(), 3, "every file listed, got {:?}", p.files);
        for (n, expected) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
            assert!(
                p.files[n].ends_with(expected),
                "file {n} should be {expected}, got {}",
                p.files[n]
            );
        }

        // Guards the promised-data fallback: it may only consult the
        // pasteboard-level read for the first item declaring a UTI, or every
        // item here would collapse onto a.txt.
        let urls: Vec<&str> = snap
            .items
            .iter()
            .map(|i| std::str::from_utf8(i.find(UTI_FILE_URL).unwrap()).unwrap())
            .collect();
        assert!(urls[1].ends_with("b.txt"), "item 1 kept its own url: {urls:?}");
        assert!(urls[2].ends_with("c.txt"), "item 2 kept its own url: {urls:?}");

        // A file list carries no text, so a plain-text paste must decline.
        assert!(text_only(&snap).is_none());
    }

    /// Image capture, against a real system-written pasteboard.
    ///
    /// Set the fixture up first — this deliberately does not write the image
    /// itself, so the capture is tested against AppKit's own encoding rather
    /// than our restore path:
    ///
    /// ```text
    /// sips -z 137 241 some.png --out /tmp/cqtest/shot.png
    /// osascript -e 'set the clipboard to (read (POSIX file "/tmp/cqtest/shot.png") as «class PNGf»)'
    /// ```
    #[test]
    #[ignore = "needs the 241x137 fixture image on the real clipboard"]
    fn live_image_captures_and_survives_a_round_trip() {
        let _guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());

        let snap = snapshot().expect("snapshot");
        assert!(snap.is_complete(), "image capture should be complete");
        let p = preview(&snap);
        assert_eq!(p.kind, "image");
        assert_eq!((p.width, p.height), (Some(241), Some(137)), "IHDR dimensions");
        assert!(p.bytes > 1000, "real payload, got {} bytes", p.bytes);

        // An image carries no text, so a plain-text paste must decline rather
        // than hand back an empty snapshot.
        assert!(text_only(&snap).is_none());

        restore(&snap).expect("restore");
        let back = snapshot().expect("re-snapshot");
        let p2 = preview(&back);
        assert_eq!(p2.kind, "image");
        assert_eq!((p2.width, p2.height), (Some(241), Some(137)));
        assert_eq!(p2.bytes, p.bytes, "byte-identical round trip");
    }

    /// A transient type must not survive into the pasteboard, but the types
    /// alongside it must.
    #[test]
    #[ignore = "overwrites the real clipboard"]
    fn live_transient_type_is_dropped_on_restore() {
        // Poisoning is irrelevant here: the lock guards ordering, not data.
        let _guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let snap = ClipSnapshot {
            items: vec![item(&[
                (UTI_TEXT, b"keep me"),
                ("org.chromium.internal.source-rfh-token", b"\x01\x02\x03\x04"),
            ])],
        };
        restore(&snap).expect("restore");
        let back = snapshot().expect("snapshot");
        let utis: Vec<String> = back.items[0].types.iter().map(|t| t.uti.clone()).collect();
        assert!(utis.iter().any(|u| u == UTI_TEXT), "text kept: {utis:?}");
        assert!(
            !utis.iter().any(|u| u.contains("rfh-token")),
            "transient token must be dropped: {utis:?}"
        );
    }

    #[test]
    fn transient_chromium_token_is_not_restored() {
        assert!(is_transient("org.chromium.internal.source-rfh-token"));
        assert!(!is_transient(UTI_TEXT));
        assert!(!is_transient("org.chromium.source-url"));
    }
}
