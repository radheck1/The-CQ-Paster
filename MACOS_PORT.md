# CQ Paster — macOS port brief

Handoff document for building the macOS version. Written from the Windows side,
where v0.5.0 is complete and shipping.

---

## 1. What the app is

**CQ Paster** is an ultra-minimal, hotkey-driven multi-slot clipboard manager.
It lives in the tray/menu bar with almost no UI.

**Core interaction (Windows, shipping today):**

| Chord | Action |
| --- | --- |
| `Ctrl + <N> + C` | Copy the current selection into slot N (1–9) |
| `Ctrl + <N> + V` | Paste slot N |
| `Ctrl + Shift + <N> + V` | Paste slot N as plain text |

Plain `Ctrl+C` / `Ctrl+V` must remain **completely unaffected**. That is a hard
requirement, not a nice-to-have.

The digit is pressed **before** the letter deliberately: the app needs to know
which slot is targeted before the action fires.

**Slots** are grouped into **folders**, each folder holding its own independent
set of 9 slots. Copy/paste/clear/undo all act on the active folder only.
The folder named **Main** is permanent and cannot be renamed or deleted.

**Two modes:** *Master* (zero UI) and *Noob* (a reference popup near the cursor).

**Accepted tradeoff:** the app claims `Ctrl+1`–`Ctrl+9` as its trigger prefix, so
browser tab-switching shortcuts stop working while it runs. The user has
explicitly accepted this. The same applies to `Cmd+1`–`Cmd+9` on macOS.

---

## 2. The macOS hotkey decision (already made — do not relitigate)

Use **Cmd (⌘) as the trigger key**, mirroring Ctrl on Windows:

- `Cmd + <N> + C` — copy into slot N
- `Cmd + <N> + V` — paste slot N
- `Cmd + Shift + <N> + V` — paste slot N as plain text

Plain `Cmd+C` / `Cmd+V` must pass through untouched.

---

## 3. Repo and current state

```
git clone https://github.com/radheck1/The-CQ-Paster.git
```

Current version: **v0.5.0**, tagged and released. Windows is feature-complete.
**No macOS work has been started.**

### Stack

- **Tauri v2** (Rust backend + webview frontend)
- Frontend is **vanilla TypeScript + Vite** — no framework, no UI library
- `bincode` for state persistence
- Windows-only crates (`rdev`, `clipboard-win`, `windows-sys`) are already
  isolated under `[target.'cfg(windows)'.dependencies]` in `Cargo.toml`

### File map

| File | Role | Portability |
| --- | --- | --- |
| `src-tauri/src/slots.rs` | `SlotStore` (9 slots), `FolderStore` (folders + active pointer), persistence, 11 unit tests | **Fully portable — do not change** |
| `src-tauri/src/lib.rs` | Tauri wiring, tray, commands, `AppState` | Mostly portable; several `#[cfg(windows)]` blocks |
| `src-tauri/src/hook.rs` | Global keyboard hook + chord state machine | **Windows-only. Needs a macOS sibling.** |
| `src-tauri/src/clipboard.rs` | Raw clipboard snapshot/restore | **Windows-only impls. Needs a macOS sibling.** Preview/parsing helpers are shared and portable. |
| `src/main.ts` | Frontend for both windows (branches on window label) | **Fully portable** |
| `src/styles.css` | Theme tokens, all UI styling | **Fully portable** |

---

## 4. Start here: confirm it already builds

The crate *should* already compile and run on macOS, giving you the full UI,
tray, folders and persistence — with **no hotkeys and no clipboard capture**.
That is your baseline. I could not verify this from Windows (no cross-compile),
so **confirm it first before writing any new code**:

```bash
npm install
npm run tauri dev
```

Why it should work: `hook.rs` has a `#[cfg(not(windows))]` no-op `start()`,
`clipboard.rs` has `#[cfg(not(windows))]` stubs for `snapshot()`/`restore()`
that return `Err`, and `lib.rs` has a `#[cfg(not(windows))]` fallback for the
tray icon. The pure byte-parsing helpers (`preview`, `text_only`, `parse_hdrop`,
`dib_dimensions`, `trim_preview`, `utf16_to_string`) are **not** gated and
compile everywhere.

If it doesn't build, fix the `cfg` gates first and commit that separately.

**Rule for the whole port: never break the Windows build.** Everything
macOS-specific goes behind `#[cfg(target_os = "macos")]`. Windows is shipping
to a real user.

---

## 5. What must be built

### 5.1 Clipboard layer (`NSPasteboard`)

Implement macOS versions of the functions `hook.rs` calls:

```rust
pub fn snapshot() -> Result<ClipSnapshot, String>
pub fn restore(snap: &ClipSnapshot) -> Result<(), String>
pub fn is_sensitive() -> bool
pub fn sequence_number() -> u32   // see §6.10 — map to NSPasteboard.changeCount
pub fn init_thread()          // may be a no-op on macOS
pub fn text_only(snap) -> Option<ClipSnapshot>   // already shared; may need mac UTI awareness
```

`ClipSnapshot` is `Vec<ClipFormat { id: u32, data: Vec<u8> }>`. On Windows `id`
is a numeric clipboard format id. **macOS uses string UTIs, not integers**, so
you need a mapping strategy. Two options:

- **Preferred:** add a `#[cfg(target_os = "macos")]` variant that stores the UTI
  string alongside the bytes. Since `ClipFormat` is serialized with `bincode`
  into the persisted store, changing its shape is a **breaking change to the
  on-disk format** — but macOS has no existing users, so on macOS you are free.
  Just don't change the Windows representation.
- Alternative: intern UTI strings to synthetic u32 ids in a side table. Simpler
  to type, worse to debug, and the ids won't survive a restart. Not recommended.

**Types worth capturing** (roughly the macOS analogue of what Windows captures):

| UTI | Meaning |
| --- | --- |
| `public.utf8-plain-text` | plain text |
| `public.html` | HTML |
| `public.rtf` | rich text |
| `public.png`, `public.tiff` | images |
| `public.file-url` / `NSFilenamesPboardType` | file lists |
| app-specific types | capture verbatim, don't interpret |

Capture **every** type the source app published, and write those exact bytes
back on restore. Do not try to interpret payloads for storage — only for the
small human-readable preview.

**Privacy — this matters.** Windows honours
`ExcludeClipboardContentFromMonitorProcessing` / `CanIncludeInClipboardHistory`
so password managers aren't captured. The macOS convention is the pasteboard
type **`org.nspasteboard.ConcealedType`**. Check for it in `is_sensitive()` and
skip the copy entirely when present. 1Password and similar set it.

### 5.2 Global hook (`CGEventTap`)

Replace the `rdev`-based Windows hook. `rdev` does have macOS support, but its
suppression story is weaker — evaluate a direct `CGEventTap` via the
`core-graphics` crate, which is what you want for reliable suppression.

You need to:
- Install a tap at `kCGHIDEventTap` / `kCGSessionEventTap` for
  `kCGEventKeyDown` (+ `kCGEventFlagsChanged` if you track modifiers via events)
- Return `None` from the callback to **swallow** an event (the digit, and the V
  in a paste chord)
- Post a synthetic `Cmd+V` with `CGEvent` to perform the actual paste

**Permissions — the biggest macOS-specific hurdle:**

- A `CGEventTap` requires **Accessibility** permission
  (System Settings → Privacy & Security → Accessibility).
- Recent macOS also gates key capture behind **Input Monitoring**.
- Prompt with `AXIsProcessTrustedWithOptions` and
  `kAXTrustedCheckOptionPrompt: true`.
- **In dev, the permission is granted to the *terminal/IDE* running the binary,
  not to the app.** This causes enormous confusion — a rebuild can silently lose
  the tap. Budget time for this and tell the user plainly when a permission
  re-grant is needed.
- The app must handle the permission being absent at launch without crashing,
  and ideally re-check when it's granted.

**`kCGEventTapDisabledByTimeout` — read this twice.** This is the direct
analogue of the Windows bug that cost us the most time. If your tap callback
takes too long, **macOS silently disables the tap** and every hotkey stops
working. You must listen for `kCGEventTapDisabledByTimeout` (and
`...ByUserInput`) and call `CGEventTapEnable` to re-arm it. See §6.1 — the
callback must stay minimal regardless.

### 5.3 Non-activating popup panel

`lib.rs` has `make_non_activating()` (`#[cfg(windows)]`, uses
`WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`) so the Noob-mode popup never steals focus
from the app being pasted into. The macOS equivalent:

- Make the popup window an `NSPanel` with `.nonactivatingPanel` style
- `collectionBehavior`: `.canJoinAllSpaces` + `.fullScreenAuxiliary`
- Set `level` above normal windows
- Ensure it never becomes key/main

Getting this wrong means the popup steals focus and the paste lands in the
wrong app — an obvious, immediate bug.

### 5.4 Paths

`lib.rs` has:

```rust
fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA") ...;
    base.join("com.cqpaster.app")
}
```

Add a `#[cfg(target_os = "macos")]` arm using
`~/Library/Application Support/com.cqpaster.app`. Everything downstream
(`folders.bin`, `slots.bin`, `autostart.init`) then works unchanged.

Note the Windows migration path reads a legacy `slots.bin`; on macOS there is
nothing to migrate, and `FolderStore::load` already handles a missing file by
returning a default store with a single **Main** folder.

### 5.5 Menu bar icon + theme

`lib.rs` reads the Windows registry to detect light/dark and swaps the tray icon
(`system_uses_light_theme`, `tray_icon_image`, `spawn_theme_watcher`, all
`#[cfg(windows)]`). On macOS, use a **template image** (`setTemplate: true`) and
the system tints it automatically for light/dark — no polling watcher needed.
Icons live in `src-tauri/icons/` (`tray-white.png`, `tray-black.png`).

### 5.6 Autostart

`tauri-plugin-autostart` is already configured with
`MacosLauncher::LaunchAgent`, so this should work with no changes. Verify it.

---

## 6. Hard-won lessons — carry these over

These were paid for with long debugging sessions on Windows. Read before coding.

### 6.1 Never put slow work in the hook callback

`hook.rs` carries this comment, and it is the single most important constraint
in the codebase:

> **IMPORTANT: keep this callback minimal and non-blocking.** A low-level
> keyboard hook that takes too long (~300ms, `LowLevelHooksTimeout`) is silently
> bypassed by Windows, which lets suppressed keys leak through.
> **Do NOT add logging or slow work here.**

A single `eprintln!` in that callback made `Ctrl+2` start switching browser tabs
again — the suppression silently stopped working. **macOS has the same failure
mode** via `kCGEventTapDisabledByTimeout`, except it disables the tap entirely
rather than degrading.

**Architecture to preserve:** the callback does nothing but classify the
keystroke and `send()` an enum down an `mpsc::channel`. A **separate worker
thread** does all clipboard work. Keep this split exactly as-is.

### 6.2 Query the modifier live, never cache it

The Windows hook calls `ctrl_physically_down()` (`GetAsyncKeyState`) on every
keypress rather than tracking Ctrl via a cached flag. Reason: a modifier release
can be **missed while injecting a synthetic paste**, and a stale cached flag then
misreads a later plain `Ctrl+C` as a slot store — **silently clobbering a saved
slot**. Use the macOS equivalent (`CGEventSourceKeyState` or the event's own
`flags`) and query it live.

### 6.3 Only cache the pending digit, and reset it on modifier press

The only cached chord state is an `AtomicUsize` holding the pending slot (0 =
none), cleared on every fresh Ctrl press so a plain `Ctrl+C` can never reuse a
stale slot. The callback must be `Fn`, not `FnMut` — hence atomics.

### 6.4 Restore must write all formats without clearing between them

On Windows, `clipboard_win::raw::set` **empties the clipboard on every call**, so
setting N formats left only the last one. Fix was to empty once, then use
`set_without_clear` per format. **The macOS analogue:** `NSPasteboard`
`clearContents()` must be called **exactly once**, then `setData:forType:` for
each type. Calling `declareTypes:` repeatedly will wipe earlier types.

### 6.5 Don't snapshot the live clipboard right before **pasting**

An earlier version snapshotted the current clipboard immediately before loading a
slot to paste it, intending to restore it afterwards. This **woke the source
app's asynchronous/lazy pasteboard provider**, which then re-asserted the
clipboard and clobbered what we had just set. That is a direct cause of the long
file-paste bug. The paste path therefore does **not** snapshot first, and must
not be "improved" to do so. macOS has lazy pasteboard providers too
(`NSPasteboardItemDataProvider`) — expect the same class of bug.

**This applies to the paste path only.** The *copy* path deliberately does
snapshot beforehand — see §6.11, which has a guard that makes it safe.

### 6.6 File pastes must be a copy, not a move

Windows needed `Preferred DropEffect = DROPEFFECT_COPY` or Finder-equivalent
treated the paste as a move ("source and destination are the same" dialogs).
Check the macOS behaviour for `public.file-url` pastes into Finder.

### 6.7 Build a standalone diagnostic binary for clipboard debugging

The file-paste bug was only cracked by writing a tiny standalone Rust binary
that dumped the pasteboard and tried restores, iterating in seconds instead of
rebuilding all of Tauri. **Do this immediately** when clipboard behaviour gets
confusing. On Windows it lived outside the repo in a scratch dir.

### 6.8 Release Shift before injecting the paste

For plain-text paste the user is physically holding Shift. The Windows code
releases Shift before injecting so the target receives a clean `Cmd/Ctrl+V`, not
`Ctrl+Shift+V` (which opens paste-special dialogs in some apps). Do the same.

### 6.9 Not every "bug" is a bug

Plain-text paste appeared broken in Google Docs. Diagnostics proved the stripping
worked correctly — **Google Docs applies destination formatting** to plain-pasted
text. Verify with a neutral target (TextEdit in plain-text mode) before chasing.

### 6.10 Never sleep a fixed guess waiting for a copy to land

When the chord fires, the `C` is passed **through** to the foreground app, which
then writes the clipboard **asynchronously**. So after passing it through, the
worker has to wait before reading — otherwise it captures the *previous*
clipboard contents and silently stores the wrong thing in the slot.

The original Windows implementation slept a flat 120ms. That is a guess, and it
fails silently in one direction: any app slower than the guess (large selection,
many files) gets the wrong content captured with no error. **It has since been
replaced** with an event-driven wait, and macOS must do the same.

The Windows version now uses `GetClipboardSequenceNumber()`. **The macOS
analogue is `NSPasteboard.general.changeCount`** — same idea, an integer that
increments on every pasteboard change, readable without taking the pasteboard.

The algorithm (see `hook.rs::wait_for_clipboard`), worth copying exactly:

1. Sample the counter **in the hook callback**, before the copy can land, and
   send it along with the action. Sampling it in the worker races the copy.
   A bare counter read is cheap enough to be the one exception to §6.1 — it
   takes no lock and only runs on an actual chord, not on every keystroke.
2. **Phase 1** — poll every 5ms until the counter differs from the baseline.
   If it never does before a ~600ms deadline, **nothing was copied** (no
   selection, or an app that ignores `Cmd+C`): return false and leave the slot
   untouched rather than storing stale clipboard content.
3. **Phase 2** — an app publishing several types bumps the counter **once per
   type**, so "changed" is not "finished". Keep polling until it holds still for
   ~40ms, then read. Without this you snapshot mid-write and capture only some
   of the types.

Note the two deadlines protect different things, and the worker is serial: an
over-long deadline stalls a subsequent paste, so don't inflate it casually.

### 6.11 A slot copy is a stash: preserve the user's clipboard

`Cmd/Ctrl + <N> + C` must fill slot N **without disturbing what a plain
`Cmd/Ctrl + V` pastes**. Since the chord works by passing the `C` through so the
app performs a real copy, the app necessarily overwrites the system clipboard —
so the previous contents have to be captured and put back.

Implemented in `hook.rs` as `capture_previous()` + a restore after the slot is
filled. The ordering is:

1. Capture the clipboard **before** the copy lands
2. Wait for the copy and store it into the slot (§6.10)
3. Restore the captured clipboard

**The guard is the whole trick.** If you capture *after* the app's copy has
already landed, you will "restore" the very thing you just stashed and silently
undo the user's copy. So the capture is only trusted when the sequence
number / `changeCount` still equals the baseline **both before and after** the
read. If it moved at any point, discard the capture and leave the clipboard
alone — degrade to not preserving rather than doing something wrong. The same
check also throws away a snapshot that is internally inconsistent (some formats
captured before the app's write, some after).

Also skip the capture entirely when the previous clipboard `is_sensitive()` —
never re-publish password-manager content that was flagged not to be kept.

**Known risk, accepted by the user:** this briefly holds the clipboard open
right as the source app wants to write to it, so a source app that doesn't retry
its open could lose the race and fail its copy. Verified working on Windows with
Explorer multi-file copies, which is the worst case. If macOS shows flaky or
empty slots after this, that race is the first suspect.

---

## 7. Tauri specifics already learned

- **Tray menu event handlers are registered globally**, not per-menu, so
  `set_menu()` with a rebuilt menu keeps firing the original handler. Verified in
  the Tauri 2.11.5 source.
- **`run_item_main_thread!` posts to the main-thread event loop and then blocks
  on `rx.recv()`.** Calling a menu setter *from* the main thread deadlocks — and
  menu-event handlers run on the main thread. `lib.rs::refresh_tray()` therefore
  **always spawns a thread**. Don't "simplify" that away.
- The frontend re-renders wholesale on `state-updated`. Renders are **deferred
  while the user is typing a folder name**, or the input gets destroyed
  mid-keystroke.
- `set_mode` deliberately **emits no event** so the mode toggle can cross-fade in
  place; the frontend updates its cached state manually to compensate.

---

## 8. Building the macOS installer

### Prerequisites

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Node 18+ via nodejs.org or: brew install node
```

### Dev

```bash
npm install
npm run tauri dev
```

### Release build

```bash
npm run tauri build
```

Outputs (`bundle.targets` is `"all"` in `tauri.conf.json`):

```
src-tauri/target/release/bundle/macos/CQ Paster.app
src-tauri/target/release/bundle/dmg/CQ Paster_0.5.0_x64.dmg
```

### Universal binary (Apple Silicon + Intel) — recommended for sharing

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run tauri build -- --target universal-apple-darwin
```

### Gatekeeper

The build is **unsigned**, so macOS will refuse to open it normally
("damaged / unidentified developer"). Recipients must either:

- Right-click the app → **Open** → **Open** in the dialog, or
- `xattr -dr com.apple.quarantine "/Applications/CQ Paster.app"`

Proper signing needs an **Apple Developer ID** ($99/yr) plus **notarization**
(`xcrun notarytool`). The Windows build has the same unsigned caveat with
SmartScreen, and the user has accepted it for now. Don't sink time into signing
unless asked.

### Publishing a release

Windows releases go to GitHub Releases with the installer attached. **A slot has
already been prepared for the macOS build** — do not create a competing release
scheme.

**Where the `.dmg` goes:** attach it to the existing release for the version you
build against, alongside the Windows `.exe`. One release, both platforms' assets.
Name it to match the Windows convention:

```
CQ_Paster_0.5.1_x64-setup.exe          <- existing Windows asset
CQ_Paster_0.5.1_universal.dmg          <- add the macOS asset like this
```

If the port lands on a later version, cut a new tag and include **both**
platforms' installers in that release.

**Two places must be updated when the Mac build first ships**, both of which
currently say "in development":

1. **`README.md`** — the *Downloads* table at the top (change the macOS row from
   🚧 to ✅ with a version and download link), and the *macOS (in development)*
   section heading. The "How the two versions will differ" table there is already
   written and should stay — it explains ⌘ vs Ctrl, `.dmg` vs `.exe`, Gatekeeper
   vs SmartScreen, and the Accessibility/Input Monitoring requirement.
2. **The GitHub release notes** — the current release has a **macOS — coming
   soon** section. Replace it with real install instructions, including the
   Gatekeeper workaround and the permissions the user must grant.

**Ask before publishing.** The user drives release timing, and releases are
public.

---

## 9. Test matrix

Verify each on macOS. Windows passes all of these.

**Chords**
- [ ] `Cmd+1+C` … `Cmd+9+C` store into the right slot (top row **and** numpad)
- [ ] `Cmd+N+V` pastes the right slot
- [ ] `Cmd+Shift+N+V` pastes stripped of formatting (test in TextEdit, not Docs)
- [ ] Plain `Cmd+C` / `Cmd+V` completely unaffected
- [ ] Plain `Cmd+C` after a chord does **not** clobber a stored slot (see §6.2)
- [ ] `Cmd+1`…`Cmd+9` swallowed — browser tabs don't switch

**Content types**
- [ ] Plain text
- [ ] Rich text / HTML (formatting survives a normal paste)
- [ ] Images (screenshot → paste into Preview/Notes)
- [ ] **Files in Finder** — copy files, paste elsewhere; must be a **copy**
- [ ] Multiple files at once
- [ ] Password manager content is **skipped** (`org.nspasteboard.ConcealedType`)
- [ ] A **slow/large** copy (big spreadsheet range, many files) captures the new
      content — not the previous clipboard contents (see §6.10)
- [ ] A chord fired with **nothing selected** leaves the slot untouched rather
      than overwriting it with stale clipboard content

**Folders**
- [ ] Slots are independent per folder
- [ ] Clear all / Undo affect only the active folder
- [ ] Create switches to the new folder
- [ ] **Main** cannot be renamed or deleted
- [ ] Menu-bar folder submenu switches folders
- [ ] Folders survive a restart

**Windows/UI**
- [ ] Noob popup appears near the cursor and **never steals focus**
- [ ] Popup shows the active folder name
- [ ] Menu bar icon looks right in light and dark
- [ ] Closing the main window keeps the app alive in the menu bar
- [ ] Start-on-login works

**Regression**
- [ ] `cargo test` still passes (11 tests in `slots.rs`)
- [ ] The **Windows** build still compiles

---

## 10. Suggested order of work

1. Confirm the current code builds and runs on macOS (UI only). Commit any
   `cfg` fixes separately.
2. `data_dir()` for macOS — cheap, unblocks persistence.
3. Menu-bar template icon.
4. **Clipboard layer** (`NSPasteboard`) with a standalone diagnostic binary
   alongside it. Text first, then images, then files.
5. **`CGEventTap` hook** + Accessibility permission flow. Get plain
   `Cmd+N+C` / `Cmd+N+V` working with text only.
6. Non-activating `NSPanel` for the Noob popup.
7. Plain-text paste variant.
8. Walk the test matrix.
9. Universal build → `.dmg`.

Work in small commits and keep Windows green throughout.
