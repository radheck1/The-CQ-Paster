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
    pub fn is_empty(&self) -> bool {
        self.items.iter().all(|i| i.types.is_empty())
    }

    /// First occurrence of `uti` across all items.
    fn find(&self, uti: &str) -> Option<&[u8]> {
        self.items.iter().find_map(|i| i.find(uti))
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
        for item in items.iter() {
            let mut types = Vec::new();
            for ty in item.types().iter() {
                let uti = ty.to_string();
                let Some(data) = item.dataForType(&ty) else {
                    continue; // unmaterialised promise
                };
                let mut data = data.to_vec();
                if uti == UTI_FILE_URL {
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
    let types: Vec<ClipType> = snap
        .items
        .iter()
        .flat_map(|i| i.types.iter())
        .filter(|t| keep.contains(&t.uti.as_str()))
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
