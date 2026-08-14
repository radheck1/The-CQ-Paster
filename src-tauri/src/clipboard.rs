//! Raw clipboard snapshot / restore.
//!
//! The whole point of a clipboard *manager* that can re-paste "anything you can
//! copy" is to capture the clipboard at the byte level across every format the
//! source app published (text, HTML, RTF, images, file lists, app-specific
//! formats) and write those exact bytes back later. We deliberately do NOT try
//! to interpret the payload for storage — only to build a small human-readable
//! preview for the Noob-mode popup.

use serde::{Deserialize, Serialize};

/// One clipboard format: the Windows format id and its raw bytes.
#[derive(Clone, Serialize, Deserialize)]
pub struct ClipFormat {
    pub id: u32,
    pub data: Vec<u8>,
}

/// A full snapshot of every (memory-backed) clipboard format at a point in time.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ClipSnapshot {
    pub formats: Vec<ClipFormat>,
}

impl ClipSnapshot {
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }
    fn find(&self, id: u32) -> Option<&[u8]> {
        self.formats.iter().find(|f| f.id == id).map(|f| f.data.as_slice())
    }
}

/// Preview metadata shown in the popup / control panel. Cheap to compute and
/// serde-serializable straight to the frontend.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlotPreview {
    /// "text" | "image" | "files" | "other"
    pub kind: String,
    pub text: Option<String>,
    pub files: Vec<String>,
    pub bytes: usize,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

// Standard clipboard format ids (winuser.h).
const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_ENHMETAFILE: u32 = 14;
const CF_HDROP: u32 = 15;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const CF_OWNERDISPLAY: u32 = 0x0080;
const CF_DSPBITMAP: u32 = 0x0082;
const CF_DSPMETAFILEPICT: u32 = 0x0083;
const CF_DSPENHMETAFILE: u32 = 0x008E;

/// Formats whose clipboard data is a GDI/handle object rather than a plain
/// GlobalAlloc block. GlobalLock-ing these gives meaningless bytes, so we skip
/// them on capture and rely on the memory-backed equivalents (CF_DIB/CF_DIBV5
/// carry the image; CF_HDROP carries files).
fn is_handle_format(id: u32) -> bool {
    matches!(
        id,
        CF_BITMAP
            | CF_METAFILEPICT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_OWNERDISPLAY
            | CF_DSPBITMAP
            | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE
    )
}

#[cfg(windows)]
pub fn snapshot() -> Result<ClipSnapshot, String> {
    let mut formats = static_snapshot()?;
    // Explorer publishes files as a "Shell IDList Array" (PIDLs) rather than a
    // static CF_HDROP. Convert it to a plain CF_HDROP so the slot holds a file
    // list that pastes anywhere, independent of the source app staying alive.
    augment_files(&mut formats);
    // Safety net for apps that publish only a device-dependent CF_BITMAP (a GDI
    // handle we can't byte-copy) with no memory-backed DIB alongside it.
    augment_bitmap(&mut formats);
    Ok(ClipSnapshot { formats })
}

/// Return a snapshot containing only the plain-text formats, dropping HTML,
/// RTF, images, and everything else — for a "paste without formatting". Returns
/// None if the slot carries no text at all (e.g. an image or file list).
pub fn text_only(snap: &ClipSnapshot) -> Option<ClipSnapshot> {
    const CF_OEMTEXT: u32 = 7;
    const CF_LOCALE: u32 = 16;
    let keep = [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT, CF_LOCALE];

    let has_text = snap
        .formats
        .iter()
        .any(|f| f.id == CF_UNICODETEXT || f.id == CF_TEXT);
    if !has_text {
        return None;
    }
    let formats: Vec<ClipFormat> = snap
        .formats
        .iter()
        .filter(|f| keep.contains(&f.id))
        .cloned()
        .collect();
    Some(ClipSnapshot { formats })
}

/// True if the current clipboard is flagged by its owner as sensitive /
/// not-for-history (password managers, banking apps, etc.). We must not store
/// such content in a slot.
#[cfg(windows)]
pub fn is_sensitive() -> bool {
    use clipboard_win::{raw, Clipboard, EnumFormats};

    let Ok(_clip) = Clipboard::new_attempts(10) else {
        return false;
    };
    let exclude = register_format("ExcludeClipboardContentFromMonitorProcessing");
    let history = register_format("CanIncludeInClipboardHistory");
    let cloud = register_format("CanUploadToCloudClipboard");
    let ids: Vec<u32> = EnumFormats::new().collect();

    // Presence alone means "don't capture".
    if exclude != 0 && ids.contains(&exclude) {
        return true;
    }
    // These carry a DWORD; a value of 0 means "no".
    let read_dword = |id: u32| -> Option<u32> {
        let size = raw::size(id)?.get();
        if size < 4 {
            return None;
        }
        let mut buf = [0u8; 4];
        raw::get(id, &mut buf).ok()?;
        Some(u32::from_le_bytes(buf))
    };
    for id in [history, cloud] {
        if id != 0 && ids.contains(&id) && read_dword(id) == Some(0) {
            return true;
        }
    }
    false
}

/// If we captured no memory-backed bitmap but the clipboard holds a device
/// bitmap (CF_BITMAP), convert that GDI handle to a packed CF_DIB.
#[cfg(windows)]
fn augment_bitmap(formats: &mut Vec<ClipFormat>) {
    if formats.iter().any(|f| f.id == CF_DIB || f.id == CF_DIBV5) {
        return; // already have a memory-backed image
    }
    use clipboard_win::Clipboard;
    let Ok(_clip) = Clipboard::new_attempts(10) else {
        return;
    };
    if let Some(dib) = dib_from_clipboard_bitmap() {
        formats.push(ClipFormat {
            id: CF_DIB,
            data: dib,
        });
    }
}

/// Convert the clipboard's CF_BITMAP (an HBITMAP) into a packed DIB
/// (BITMAPINFOHEADER + 32bpp pixels). Assumes the clipboard is already open.
#[cfg(windows)]
fn dib_from_clipboard_bitmap() -> Option<Vec<u8>> {
    use core::mem::{size_of, zeroed};
    use windows_sys::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        DIB_RGB_COLORS,
    };
    use windows_sys::Win32::System::DataExchange::GetClipboardData;

    const CF_BITMAP_ID: u32 = 2;
    let handle = unsafe { GetClipboardData(CF_BITMAP_ID) };
    if handle.is_null() {
        return None;
    }

    let mut bm: BITMAP = unsafe { zeroed() };
    let ok =
        unsafe { GetObjectW(handle as _, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) };
    if ok == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        return None;
    }
    let width = bm.bmWidth;
    let height = bm.bmHeight.abs();

    let mut bi: BITMAPINFO = unsafe { zeroed() };
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = width;
    bi.bmiHeader.biHeight = height; // bottom-up DIB
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = 0; // BI_RGB

    let stride = (((width * 32 + 31) / 32) * 4) as usize;
    let img_size = stride * height as usize;
    let mut bits = vec![0u8; img_size];

    let hdc = unsafe { GetDC(core::ptr::null_mut()) };
    if hdc.is_null() {
        return None;
    }
    let scan = unsafe {
        GetDIBits(
            hdc,
            handle as _,
            0,
            height as u32,
            bits.as_mut_ptr() as *mut _,
            &mut bi as *mut _,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        ReleaseDC(core::ptr::null_mut(), hdc);
    }
    if scan == 0 {
        return None;
    }

    bi.bmiHeader.biSizeImage = img_size as u32;
    let mut out = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + img_size);
    let hdr = unsafe {
        core::slice::from_raw_parts(
            &bi.bmiHeader as *const BITMAPINFOHEADER as *const u8,
            size_of::<BITMAPINFOHEADER>(),
        )
    };
    out.extend_from_slice(hdr);
    out.extend_from_slice(&bits);
    Some(out)
}

/// Capture every memory-backed format currently sitting on the clipboard.
#[cfg(windows)]
fn static_snapshot() -> Result<Vec<ClipFormat>, String> {
    use clipboard_win::{raw, Clipboard, EnumFormats};

    let _clip = Clipboard::new_attempts(10).map_err(|e| format!("open clipboard: {e}"))?;
    let mut formats = Vec::new();
    for id in EnumFormats::new() {
        if id == 0 || is_handle_format(id) {
            continue;
        }
        let size = match raw::size(id) {
            Some(s) => s.get(),
            None => continue,
        };
        let mut buf = vec![0u8; size];
        match raw::get(id, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                formats.push(ClipFormat { id, data: buf });
            }
            Err(_) => continue,
        }
    }
    Ok(formats)
}

#[cfg(windows)]
pub fn restore(snap: &ClipSnapshot) -> Result<(), String> {
    use clipboard_win::{raw, Clipboard};

    let _clip = Clipboard::new_attempts(10).map_err(|e| format!("open clipboard: {e}"))?;
    raw::empty().map_err(|e| format!("empty clipboard: {e}"))?;

    // NOTE: `raw::set` empties the clipboard on every call, so setting multiple
    // formats with it leaves only the last one. We empty once above, then use
    // `set_without_clear` for each format.

    // File slots: publish a clean CF_HDROP + copy effect only. Restoring the
    // shell/OLE plumbing formats makes the shell fight us for the clipboard.
    if let Some(hdrop) = snap.find(CF_HDROP) {
        let _ = raw::set_without_clear(CF_HDROP, hdrop);
        // Always paste files as a COPY — our hotkey is a copy, not a cut. This
        // also avoids the "source and destination are the same" move-onto-self
        // error when pasting back into the originating folder.
        let pde = register_format("Preferred DropEffect");
        let _ = raw::set_without_clear(pde, &1u32.to_le_bytes()); // DROPEFFECT_COPY
        return Ok(());
    }

    for f in &snap.formats {
        // Skip dead OLE marshalling cookies — restoring their stale bytes makes
        // consumers try to unmarshal a data object that no longer exists.
        if is_ole_cookie(f.id) {
            continue;
        }
        // Some synthesized formats can reject a manual set; that's fine, the OS
        // re-synthesizes them from the primary formats we did set.
        let _ = raw::set_without_clear(f.id, &f.data);
    }
    Ok(())
}

/// Initialize COM (STA) for the calling thread. `SHGetPathFromIDListW` and
/// other shell calls require it on non-main threads. Idempotent.
#[cfg(windows)]
pub fn init_thread() {
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        let _ = CoInitializeEx(core::ptr::null(), COINIT_APARTMENTTHREADED as u32);
    }
}

/// If the clipboard holds a shell file selection ("Shell IDList Array") but no
/// static CF_HDROP, synthesize CF_HDROP (+ "Preferred DropEffect" = copy) from
/// it so the file list pastes anywhere.
#[cfg(windows)]
fn augment_files(formats: &mut Vec<ClipFormat>) {
    if formats.iter().any(|f| f.id == CF_HDROP) {
        return; // already have a static file list
    }
    let shell_fmt = register_format("Shell IDList Array");
    let cida = formats
        .iter()
        .find(|f| f.id == shell_fmt)
        .map(|f| f.data.clone());
    let Some(cida) = cida else { return };
    let Some(hdrop) = hdrop_from_shell_idlist(&cida) else {
        return;
    };
    formats.push(ClipFormat {
        id: CF_HDROP,
        data: hdrop,
    });
    let pde = register_format("Preferred DropEffect");
    if pde != 0 && !formats.iter().any(|f| f.id == pde) {
        // DROPEFFECT_COPY = 1
        formats.push(ClipFormat {
            id: pde,
            data: 1u32.to_le_bytes().to_vec(),
        });
    }
}

/// Parse a CFSTR_SHELLIDLIST (CIDA) blob into a CF_HDROP payload.
///
/// CIDA = `{ u32 cidl; u32 aoffset[cidl+1]; }` where `aoffset[0]` is the parent
/// folder PIDL and `aoffset[1..=cidl]` are child PIDLs relative to it; all
/// offsets are from the start of the blob. A full item PIDL is parent (minus
/// its null terminator) concatenated with the child PIDL.
#[cfg(windows)]
fn hdrop_from_shell_idlist(cida: &[u8]) -> Option<Vec<u8>> {
    if cida.len() < 8 {
        return None;
    }
    let cidl = u32::from_le_bytes(cida[0..4].try_into().ok()?) as usize;
    if cidl == 0 || cidl > 1000 || cida.len() < 4 + (cidl + 1) * 4 {
        return None;
    }
    let offset = |i: usize| -> usize {
        let b = 4 + i * 4;
        u32::from_le_bytes([cida[b], cida[b + 1], cida[b + 2], cida[b + 3]]) as usize
    };
    // Returns the bytes of the ITEMIDLIST at `o`, including its 2-byte null end.
    let idlist = |o: usize| -> Option<&[u8]> {
        if o >= cida.len() {
            return None;
        }
        let mut pos = o;
        loop {
            if pos + 2 > cida.len() {
                return None;
            }
            let cb = u16::from_le_bytes([cida[pos], cida[pos + 1]]) as usize;
            if cb == 0 {
                return Some(&cida[o..pos + 2]);
            }
            pos += cb;
        }
    };

    let parent = idlist(offset(0))?;
    let parent_no_term = &parent[..parent.len().saturating_sub(2)];

    let mut paths: Vec<Vec<u16>> = Vec::new();
    for i in 1..=cidl {
        let Some(item) = idlist(offset(i)) else {
            continue;
        };
        let mut combined = Vec::with_capacity(parent_no_term.len() + item.len());
        combined.extend_from_slice(parent_no_term);
        combined.extend_from_slice(item);
        if let Some(p) = path_from_pidl(&combined) {
            paths.push(p);
        }
    }
    if paths.is_empty() {
        return None;
    }
    Some(build_hdrop(&paths))
}

#[cfg(windows)]
fn path_from_pidl(pidl_bytes: &[u8]) -> Option<Vec<u16>> {
    use windows_sys::Win32::UI::Shell::SHGetPathFromIDListW;
    let mut buf = [0u16; 260]; // MAX_PATH
    let ok = unsafe { SHGetPathFromIDListW(pidl_bytes.as_ptr() as *const _, buf.as_mut_ptr()) };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(buf[..len].to_vec())
}

/// Build a CF_HDROP payload (DROPFILES header + double-null wide path list).
#[cfg(windows)]
fn build_hdrop(paths: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_le_bytes()); // pFiles: list starts after header
    out.extend_from_slice(&0u32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0u32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide = TRUE (UTF-16 paths)
    for p in paths {
        for &u in p {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // terminate this path
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // final list terminator
    out
}

#[cfg(windows)]
fn register_format(name: &str) -> u32 {
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
}

/// OLE clipboard plumbing formats that must not be restored as static bytes.
#[cfg(windows)]
fn is_ole_cookie(id: u32) -> bool {
    if id < 0xC000 {
        return false;
    }
    matches!(
        format_name(id).as_deref(),
        Some("DataObject") | Some("DataObjectAttributes") | Some("Ole Private Data")
    )
}

#[cfg(windows)]
fn format_name(id: u32) -> Option<String> {
    let standard = match id {
        CF_TEXT => Some("CF_TEXT"),
        CF_BITMAP => Some("CF_BITMAP"),
        CF_METAFILEPICT => Some("CF_METAFILEPICT"),
        CF_DIB => Some("CF_DIB"),
        CF_PALETTE => Some("CF_PALETTE"),
        CF_UNICODETEXT => Some("CF_UNICODETEXT"),
        CF_ENHMETAFILE => Some("CF_ENHMETAFILE"),
        CF_HDROP => Some("CF_HDROP"),
        CF_DIBV5 => Some("CF_DIBV5"),
        7 => Some("CF_OEMTEXT"),
        16 => Some("CF_LOCALE"),
        _ => None,
    };
    if let Some(s) = standard {
        return Some(s.to_string());
    }
    if id < 0xC000 {
        return None; // reserved/handle format with no registered name
    }
    use windows_sys::Win32::System::DataExchange::GetClipboardFormatNameW;
    let mut buf = [0u16; 256];
    let len = unsafe { GetClipboardFormatNameW(id, buf.as_mut_ptr(), buf.len() as i32) };
    if len > 0 {
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn snapshot() -> Result<ClipSnapshot, String> {
    Err("clipboard capture not implemented on this platform".into())
}

#[cfg(not(windows))]
pub fn restore(_snap: &ClipSnapshot) -> Result<(), String> {
    Err("clipboard restore not implemented on this platform".into())
}

/// Build a small preview from a snapshot for display purposes.
pub fn preview(snap: &ClipSnapshot) -> SlotPreview {
    let bytes: usize = snap.formats.iter().map(|f| f.data.len()).sum();

    // Files take priority — a file copy usually also carries a text path.
    if let Some(data) = snap.find(CF_HDROP) {
        let files = parse_hdrop(data);
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
    }

    // Images: prefer DIBv5, fall back to DIB. Pull dimensions from the header.
    if let Some(data) = snap.find(CF_DIBV5).or_else(|| snap.find(CF_DIB)) {
        let (w, h) = dib_dimensions(data);
        return SlotPreview {
            kind: "image".into(),
            text: None,
            files: Vec::new(),
            bytes,
            width: w,
            height: h,
        };
    }

    // Text.
    if let Some(data) = snap.find(CF_UNICODETEXT) {
        return SlotPreview {
            kind: "text".into(),
            text: Some(trim_preview(&utf16_to_string(data))),
            files: Vec::new(),
            bytes,
            width: None,
            height: None,
        };
    }
    if let Some(data) = snap.find(CF_TEXT) {
        let s = String::from_utf8_lossy(data);
        return SlotPreview {
            kind: "text".into(),
            text: Some(trim_preview(s.trim_end_matches('\0'))),
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

fn trim_preview(s: &str) -> String {
    let s = s.trim_matches('\0');
    let flat: String = s.chars().map(|c| if c == '\r' { ' ' } else { c }).collect();
    let mut out: String = flat.chars().take(240).collect();
    if flat.chars().count() > 240 {
        out.push('\u{2026}'); // ellipsis
    }
    out
}

fn utf16_to_string(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&u16s)
}

/// Read width/height from a BITMAPINFOHEADER (CF_DIB) or BITMAPV5HEADER.
fn dib_dimensions(data: &[u8]) -> (Option<i32>, Option<i32>) {
    if data.len() < 12 {
        return (None, None);
    }
    let w = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let h = i32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    (Some(w), Some(h.abs()))
}

/// Parse a CF_HDROP payload (a DROPFILES header followed by a double-null
/// terminated list of paths) into file path strings.
fn parse_hdrop(data: &[u8]) -> Vec<String> {
    if data.len() < 20 {
        return Vec::new();
    }
    let p_files = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let f_wide = i32::from_le_bytes([data[16], data[17], data[18], data[19]]) != 0;
    if p_files >= data.len() {
        return Vec::new();
    }
    let bytes = &data[p_files..];
    let mut files = Vec::new();

    if f_wide {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let mut cur: Vec<u16> = Vec::new();
        for &u in &units {
            if u == 0 {
                if cur.is_empty() {
                    break; // second consecutive null terminates the list
                }
                files.push(String::from_utf16_lossy(&cur));
                cur.clear();
            } else {
                cur.push(u);
            }
        }
    } else {
        for part in bytes.split(|&b| b == 0) {
            if part.is_empty() {
                break;
            }
            files.push(String::from_utf8_lossy(part).into_owned());
        }
    }
    files
}
