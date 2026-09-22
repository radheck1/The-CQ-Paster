# CQ Paster — macOS port

Originally a handoff brief written from the Windows side, before any macOS work
existed. **The port is now built and working**, so this has been revised into a
record of how macOS actually behaves — including the places the original
predictions were wrong, which are flagged as they come up. Those corrections are
the most useful part of this document.

Status: macOS works end to end on a signed, installed build, shipping as v0.6.0
with Jotter, Shotter, the arrow-key folder switch and shake to open. Windows is
unchanged since v0.5.2 and still shipping.

---

## 1. What the app is

**CQ Paster** is an ultra-minimal, hotkey-driven multi-slot clipboard manager.
It lives in the tray/menu bar with almost no UI.

| Chord | Windows | macOS |
| --- | --- | --- |
| Copy selection into slot N | `Ctrl + <N> + C` | `Cmd + <N> + C` |
| Paste slot N | `Ctrl + <N> + V` | `Cmd + <N> + V` |
| Paste slot N as plain text | `Ctrl + Shift + <N> + V` | `Cmd + Shift + <N> + V` |

Plain `Ctrl+C`/`Cmd+C` and `Ctrl+V`/`Cmd+V` must remain **completely
unaffected**. That is a hard requirement, not a nice-to-have.

The digit is pressed **before** the letter deliberately: the app needs to know
which slot is targeted before the action fires.

**Slots** are grouped into **folders**, each folder holding its own independent
set of 9 slots. Copy/paste/clear/undo all act on the active folder only.
The folder named **Main** is permanent and cannot be renamed or deleted.

**Modes (Windows only):** *Master* (zero UI) and *Noob* (a reference popup near
the cursor). macOS has no modes — the popup always shows — and its control panel
also hosts **CQ Jotter**, a notepad (§7.4).

**Accepted tradeoff:** the app claims `Ctrl+1`–`Ctrl+9` / `Cmd+1`–`Cmd+9` as its
trigger prefix, so browser tab-switching shortcuts stop working while it runs.

---

## 2. Repo layout

Two platforms, deliberately **not** factored into shared code. Windows is
shipping and cannot be re-verified from a macOS machine, so the duplication buys
the guarantee that macOS work cannot affect it.

| File | Role |
| --- | --- |
| `src-tauri/src/slots.rs` | `SlotStore`, `FolderStore`, persistence. Portable; only its test helpers are platform-specific. |
| `src-tauri/src/lib.rs` | Tauri wiring, tray, commands, window chrome, `AppState` |
| `src-tauri/src/hook.rs` | Dispatch + the Windows hook (`windows_impl`) |
| `src-tauri/src/hook/macos.rs` | `CGEventTap` + the macOS chord machine |
| `src-tauri/src/clipboard.rs` | Dispatch + the Windows clipboard layer |
| `src-tauri/src/clipboard/macos.rs` | `NSPasteboard` layer |
| `src-tauri/src/permissions.rs` | macOS permission flow (Accessibility, Input Monitoring) |
| `src/main.ts` | Frontend for every window; `MOD` renders `Ctrl` or `⌘`. On macOS, also the Paster / Jotter / Shotter switch in the title bar |
| `src/jotter.ts` | macOS only: Jotter's folders, notes and editor |
| `src/outline.ts` | The note model — lines with depths; every edit a pure function |
| `src-tauri/src/jotter.rs` | macOS only: Jotter storage, `jotter.json` |
| `src-tauri/src/reminders.rs` | macOS only: reminder schedule, card window, sound, menu-bar dot |
| `src/reminders.ts` | macOS only: the clock button's settings menu, and the reminder card |
| `src-tauri/capabilities/reminder.json` | macOS only (`platforms`): lets the card's window receive events |
| `src-tauri/src/shotter.rs` | macOS only: watching for screenshots, `shotter.json`, copy, trash, saving markup, the markup window |
| `src/shotter.ts` | macOS only: Shotter's list of screenshots |
| `src/markup.ts`, `src/markup-geom.ts` | macOS only: the markup window, and its geometry as pure functions |
| `src-tauri/capabilities/markup.json` | macOS only (`platforms`): the markup window |
| `src/styles.css` | Theme tokens; macOS-specific rules scoped to `[data-platform="macos"]` |

**Gating rule:** everything macOS-specific is behind `#[cfg(target_os = "macos")]`,
`[data-platform="macos"]` in CSS, or `IS_MAC` in the frontend. Items that used to be shared and are now
Windows-shaped use `#[cfg(not(target_os = "macos"))]`, **not** `#[cfg(windows)]`,
so the Linux build keeps compiling as it did before.

macOS deps (`objc2`, `objc2-app-kit`, `objc2-foundation`) live in a
`[target.'cfg(target_os = "macos")'.dependencies]` block. Windows crates
(`rdev`, `clipboard-win`, `windows-sys`) stay in theirs.

---

## 3. The clipboard layer

### 3.1 The shape is different, and it matters

> **Original prediction:** store the UTI string alongside the bytes, keeping
> `ClipSnapshot` a flat list of formats.
>
> **Reality:** that is not enough. A macOS pasteboard is a **list of items**,
> each with its own set of UTIs. Finder represents a three-file copy as **three
> items** carrying `public.file-url`, and publishes no `NSFilenamesPboardType`
> alongside it — there is no single-blob equivalent of `CF_HDROP` to fall back
> on.

Flattening items into one list is not a lossless simplification: setting the
same UTI twice on one pasteboard keeps only the first value **and still reports
success**, so a three-file copy silently pastes as one file. Same class of
failure as the Windows `raw::set` bug in §5.4.

So macOS keeps items as the outer dimension:

```rust
ClipSnapshot { items: Vec<ClipItem> }
ClipItem     { types: Vec<ClipType> }
ClipType     { uti: String, data: Vec<u8> }
```

This changes the persisted `bincode` layout on macOS only, which is safe because
macOS had no existing users. The Windows representation is untouched.

### 3.2 Promised data — the single biggest gotcha

`NSPasteboardItem.dataForType:` returns an **empty `NSData`** for lazily-provided
types. `NSPasteboard.dataForType:` — the pasteboard-level call — makes the owning
app actually produce the bytes.

WebKit apps (Safari, anything Electron-adjacent using WKWebView) publish their
text and HTML this way. Read them per-item and you capture only
`com.apple.webarchive`, so the slot previews blank and pastes nothing. This cost
several debugging rounds because every symptom pointed at timing instead.

The fallback is restricted to the **first item declaring a given UTI**: the
pasteboard-level call always answers from that item, so applying it blindly
copies item 0's payload across every item and turns a three-file Finder copy
into the same file three times.

### 3.3 A capture is not finished when the counter moves

`NSPasteboard.changeCount` increments when the source app calls
`clearContents`/`declareTypes` — *before* it writes any payloads. Capturing on
the bump alone finds the types declared and empty.

So the copy path waits on the **outcome**, not a duration: poll until a snapshot
has no declared-but-empty types (`is_complete()`). "Some type has bytes" is too
weak a test — WebKit lands its own types first and leaves `public.html` and
`public.utf8-plain-text` at 0 bytes for a moment.

There is **no fixed delay in the copy path**, unlike the Windows 120 ms sleep.
The 500 ms is only a give-up point.

If a publisher never fills a type it declared, keep what did arrive minus the
empty types. Never store empty payloads: they make previews blank, make
`text_only` produce a text type with no text, and paste nothing.

### 3.4 Finder publishes references, not paths

```
file:///.file/id=6571367.46089685
  -> /Users/…/Desktop/Screenshot 2026-08-13 at 9.53.16 AM.png
```

Those are volume-id references, only meaningful while the file stays put — and
slots persist across restarts. They are resolved to concrete paths at capture
time with `NSURL.filePathURL`, which is the direct analogue of the Windows
PIDL→path conversion in `augment_files`.

### 3.5 Types worth knowing

| UTI | Notes |
| --- | --- |
| `public.utf8-plain-text` | the main text type |
| `public.utf16-external-plain-text` | Finder attaches this; has a BOM, either endianness |
| `public.html` | what Chrome publishes; **no `public.rtf`** |
| `public.png` | screenshots arrive as a single PNG item; dimensions from the IHDR chunk |
| `public.file-url` | one per item, see §3.4 |
| `com.apple.webarchive` | WebKit lands this first, see §3.2 |
| `org.chromium.web-custom-data` | ~15 KB per copy, even for a few characters |
| `org.chromium.internal.source-rfh-token` | **skip on restore** — see below |

`org.chromium.internal.source-rfh-token` is the macOS `is_ole_cookie`: a
process-scoped handle identifying a render frame that is long gone by the time a
slot is pasted. Its sibling `org.chromium.source-url` records the **source page
URL** into every persisted slot, which Windows slots do not do — a privacy
wrinkle worth a deliberate decision.

### 3.6 Privacy — weaker than Windows, unavoidably

`org.nspasteboard.ConcealedType` is the only convention macOS offers, and it is
advisory. Diagnostics during the port confirmed a real password copied through
1Password's **web** interface arrives with no marker at all — just
`public.utf8-plain-text` and Chromium's source types — and **will** be captured
into a slot.

Windows has firmer ground (`ExcludeClipboardContentFromMonitorProcessing` and
friends). This is genuinely weaker on macOS, not an oversight. The test-matrix
line "password manager content is skipped" is **not fully deliverable**.
Anything stronger — a source-application denylist, say — is a product decision.

---

## 4. The hook

### 4.1 Two permissions, not one

> **Original prediction:** mentioned Input Monitoring in passing.
>
> **Reality:** it is a hard requirement and the failure is silent.

- **Accessibility** — needed to *create* the `CGEventTap`
- **Input Monitoring** — needed to *receive* events through it

A tap created before Input Monitoring is granted is created **successfully**,
returns no error, and then never delivers a single event. Granting the
permission afterwards does not revive it. Since the two are granted seconds
apart, waiting only on Accessibility means the tap is almost always built in
that dead window.

**Wait for both before creating the tap.** Check Accessibility with
`AXIsProcessTrusted`, Input Monitoring with `IOHIDCheckAccess` (`0` granted,
`1` denied, `2` unknown — no record).

Only Accessibility has a system prompt that reliably surfaces from a background
app, so `permissions.rs` shows a native alert per permission, in order, each
deep-linking to its pane:

```
x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility
x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent
```

Ask **once per launch**. An earlier version re-prompted every 30s and was
unusable — it interrupted the user while they were in System Settings granting
the very thing it was asking for.

### 4.2 Raw FFI, not the `core-graphics` wrapper

Suppressing a key requires the callback to return `NULL`. The safe `CGEventTap`
wrapper returns the *original* event when its closure yields `None`, so it can
rewrite events but can never swallow one — precisely what `Cmd+<N>` needs.

Tap at `kCGSessionEventTap` with `kCGEventTapOptionDefault`. Listen for
`kCGEventKeyDown` and `kCGEventFlagsChanged`. Post the synthetic paste with
`CGEventPost(kCGHIDEventTap, …)`.

### 4.3 Re-arm the tap

`kCGEventTapDisabledByTimeout` and `…ByUserInput` must be caught in the callback
and answered with `CGEventTapEnable`. This is the analogue of the Windows
`LowLevelHooksTimeout` bypass, except macOS disables the tap **entirely** rather
than degrading.

### 4.4 Consume the chord digit, don't clear it on release

The Windows hook clears the pending digit on a fresh Ctrl press. macOS does the
same on a fresh Cmd press, **but that is only a second line of defence**: the
digit is consumed with `swap(0)` the moment C or V uses it.

This is stronger than the Windows approach. Correctness no longer depends on
observing a modifier event at all — which is exactly the state that goes missing
while a synthetic paste is in flight (§5.2).

### 4.5 Auto-repeat re-arms a consumed chord

Holding `Cmd+<N>` repeats the digit at the OS repeat rate. A repeat arriving
*after* C or V consumed the slot silently re-arms it, turning the user's next
plain `Cmd+V` into another chord paste — invisible except in a trace.

Arm on the initial press only: check `kCGKeyboardEventAutorepeat`. Repeats are
still swallowed, so the app keeps owning `Cmd+1`–`Cmd+9`.

### 4.6 The two-half clipboard contract

Both platforms now guarantee the same thing (`MACOS_PORT.md` §6.12 on `main`
states the contract; this is the macOS implementation of it):

- `Cmd + C` → `Cmd + V` always round-trips **the user's own clipboard**
- `Cmd + <N> + C` → `Cmd + <N> + V` always round-trips **slot N**
- Neither pair ever disturbs the other

It takes two halves, and both are implemented:

**Copy stashes.** The chord passes `C` through, so the app overwrites the
pasteboard. The user's clipboard is captured first and put back once the slot is
filled.

**Paste borrows.** See §4.7.

Both use `capture_stable(expected)`, the direct port of the Windows function of
the same name: `changeCount` must match `expected` (when given) before the read
and be unchanged after it, or the capture is discarded. That guard is the whole
trick — a capture taken *after* the app's copy landed is the very content being
preserved, so restoring it would silently undo the user's copy.

**One deliberate difference from Windows.** Windows samples its baseline
sequence number inside the hook callback, where the read is a cheap local call.
`changeCount` is an IPC to the pasteboard server, and §5.1 makes that an
unacceptable risk in a tap callback, so macOS samples it at the top of the
worker instead. The cost is a race the app can win, which `capture_stable` then
rejects — degrading to *not* preserving rather than preserving the wrong thing.

### 4.7 Give the clipboard back after a paste

Pasting a slot works by writing it to the system pasteboard and injecting
`Cmd+V`. Nothing restores what was there before unless you do it, so slot N
stays on the clipboard and the user's next plain `Cmd+V` re-pastes it. The two
pairs must stay independent.

The pasteboard is snapshotted before being borrowed and handed back after.

**The handback must also check the pasteboard is still ours.** Record
`changeCount` right after our own write; if it differs when the delay expires,
something else changed the clipboard while we waited — the source app
re-asserting, or the user copying something new inside the window — and that
content is current now and must win. Skipping the handback there is the
difference between preserving a clipboard and silently stomping a copy the user
just made.

**On the hazard this reintroduces** (§5.5, and §6.5 on `main`): reading the
clipboard at paste time is what the paste path used to avoid. It is now done on
both platforms, guarded by `capture_stable`, and **verified clear on Windows**
against a real Explorer multi-file copy — the exact case that produced the
original bug.

Note that a *synthetic* file list does not test this at all. The hazard only
exists when a real source app owns the clipboard with a live lazy provider, so a
hand-built `CF_HDROP` or multi-item `public.file-url` snapshot passes while
proving nothing. The `#[ignore]`d live tests in `clipboard/macos.rs` are in that
category — useful for structure, useless for this.

This reintroduces what §5.5 records as removed on Windows — reading a lazy
provider can make the source app re-assert and clobber. macOS has the same
mechanism, and §3.2 proves those providers are real and load-bearing here. Treat
intermittent wrong-content pastes as this until proven otherwise.

The handback is the **one remaining real delay** (180 ms) on either platform:
neither macOS nor Windows has a "paste completed" signal, and reading the
clipboard does not bump the counter, so there is nothing to wait on. Windows
records one escape hatch that macOS has no analogue for — delayed rendering, so
`WM_RENDERFORMAT` fires exactly when the target asks for the data. Unbuilt on
both. Too short and the target pastes the handed-back
content; too long and a fast plain `Cmd+V` beats it.

### 4.8 Arrows switch folders during a chord

After `Cmd+<N>` arms a slot, `←` and `→` step through the folders in menu order,
wrapping round. The callback does what it does for a digit: one atomic read
(`pending_slot`), a channel send (`Action::StepFolder`), and swallows the key,
repeats included, so it's one folder per press and the app never sees `Cmd+←`.
With no slot armed, arrows pass straight through.

The slot stays armed, so `C` or `V` acts on slot N of the folder switched to. The
worker selects the folder, then persists and emits `state-updated`, which the
popup and the control panel redraw from. The wrap-around is `stepped_folder`, a
pure function with tests.

**macOS only.** Windows' hook would take the same shape, but can't be run from
here. The popup's chevrons and its arrow hint render only on macOS. The Windows
popup's HTML was checked unchanged.

---

## 5. Hard-won lessons

Carried from Windows, plus what macOS added.

### 5.1 Never put slow work in the hook callback

> **IMPORTANT: keep this callback minimal and non-blocking.**

A single `eprintln!` in the Windows callback made `Ctrl+2` start switching
browser tabs again. **macOS has the same failure mode** via
`kCGEventTapDisabledByTimeout`, except it disables the tap entirely.

Architecture: the callback classifies the keystroke and `send()`s an enum down an
`mpsc::channel`. A **separate worker thread** does all clipboard work. The only
things added to the macOS callback are two atomic increments (a chord-reset
counter and an event counter). Nothing else.

### 5.2 Query the modifier live, never cache it

A modifier release can be **missed while injecting a synthetic paste**, and a
stale cached flag then misreads a later plain `Cmd+C` as a slot store —
**silently clobbering a saved slot**. Read the modifier from each event's own
flags. See also §4.4, which removes the dependency entirely.

### 5.3 A failed capture must not overwrite a good slot

If nothing usable arrives, leave the slot untouched. Destroying saved content
because a copy did not land is strictly worse than doing nothing.

### 5.4 Restore must write all formats without clearing between them

On Windows, `clipboard_win::raw::set` **empties the clipboard on every call**.
On macOS, `clearContents()` must be called **exactly once**, then all items
written in a single `writeObjects`. Rebuilding items rather than calling
`setData:forType:` per type is what preserves multi-file copies.

### 5.5 Don't snapshot the live clipboard right before pasting

An earlier Windows version did this and **woke the source app's lazy provider**,
which re-asserted and clobbered. See §4.6 — macOS now does it deliberately, with
that risk accepted and documented.

### 5.6 File pastes must be a copy, not a move

Windows needed `Preferred DropEffect = DROPEFFECT_COPY`. macOS needs no
equivalent: pasting `public.file-url` items into Finder copies. Verified.

### 5.7 Release Shift before injecting the paste

For plain-text paste the user is physically holding Shift. Release it before
injecting so the target receives a clean `Cmd+V`.

### 5.8 Not every "bug" is a bug

Plain-text paste appeared broken in Google Docs. Diagnostics proved the stripping
worked — **Google Docs applies destination formatting**. Verify with a neutral
target (TextEdit in plain-text mode) before chasing.

### 5.9 Build standalone diagnostic binaries

The pasteboard work was cracked by a scratch `pbdiag` binary outside the repo
that dumped the pasteboard, probed item-level versus pasteboard-level reads,
restored snapshots, and injected a bare `Cmd+V`. Seconds per iteration instead of
a full Tauri rebuild.

Two findings came *only* from it: the promised-data asymmetry (§3.2), and that
synthetic `Cmd+V` injection works fine in isolation — which is what ruled out an
entire branch of theories.

**Do this immediately** when behaviour gets confusing.

### 5.10 A bundled `.app` has nowhere to print

GUI stderr is not captured by the unified log. Once installed, the app cannot
report anything about itself — which is exactly when the permission problems
happen. `lib.rs::diag()` appends to
`~/Library/Application Support/com.cqpaster.app/diagnostics.log`.

It records the executable path, both permissions with the raw IOKit value,
whether `CGEventTapCreate` returned NULL, and a watchdog counting events the tap
actually delivers. That last one separates three cases that are identical from
outside: a tap never created, a tap created but starved, and a chord machine
misreading events that are arriving fine.

**Build this before debugging an installed build, not after.**

### 5.11 AppKit calls must be on the main thread

`NSWindow` calls from the worker thread terminated the process on the first
chord — clean exit, no panic, nothing logged. Marshal with
`AppHandle::run_on_main_thread`, which posts and returns.

Distinguish this from §6: the *blocking* main-thread helpers behind Tauri's menu
setters deadlock when called **from** the main thread. Different API, opposite
hazard.

### 5.12 Ad-hoc signing invalidates permissions on every build

An ad-hoc signed app has no stable code identity — its designated requirement is
keyed on `cdhash`, which changes with every build. macOS keys Accessibility and
Input Monitoring to that identity, so **every rebuild silently invalidates
permissions the user already granted**. The System Settings entry still looks
enabled while granting nothing, and from inside the app that is
indistinguishable from never having been granted.

This made a working dev build completely inert once installed, and cost more
time than any other single issue.

Signing with a self-signed certificate gives:

```
designated => identifier "com.cqpaster.app" and certificate root = H"…"
```

Both halves survive a rebuild, so a permission granted once persists. See §7.2.

### 5.13 An old mounted DMG will be reinstalled by accident

Several builds shipped with the identical filename. An earlier volume stayed
mounted at `/Volumes/CQ Paster`, so the new one mounted as `/Volumes/CQ Paster 1`
and the app got dragged across from the **stale** volume — repeatedly, while
every symptom pointed elsewhere.

Before diagnosing an installed build, always confirm what is actually installed:

```bash
codesign -d -r- "/Applications/CQ Paster.app" 2>&1 | tail -1
```

Stamping the DMG filename with a version or build id would prevent this.

### 5.14 A replaced .app keeps running the old binary

macOS keeps a running process alive when its bundle is overwritten. Installing
over a running CQ Paster leaves the **old** binary running with the new one never
launched. Quit before installing.

---

## 6. Tauri specifics

- **Tray menu event handlers are registered globally**, not per-menu, so
  `set_menu()` with a rebuilt menu keeps firing the original handler.
- **`run_item_main_thread!` posts to the main-thread event loop and then blocks
  on `rx.recv()`.** Calling a menu setter *from* the main thread deadlocks — and
  menu-event handlers run on the main thread. `refresh_tray()` therefore always
  spawns a thread. Don't "simplify" that away. Contrast with §5.11.
- **`set_decorations(true)` does not apply in time to build on.** Reading the
  style mask afterwards showed `Titled` and `Closable` still absent. Compose the
  mask explicitly instead.
- The frontend re-renders wholesale on `state-updated`. Renders are **deferred
  while the user is typing a folder name**. In Jotter view the update is only
  stored: a redraw would replace the note under the caret (§7.4).
- `set_mode` (Windows) deliberately **emits no event** so the mode toggle can
  cross-fade.

---

## 7. macOS platform notes

### 7.1 Windows, chrome and the menu bar

- **`data_dir()`** is `~/Library/Application Support/com.cqpaster.app`.
  `SlotStore::save` discards its errors, so a bad path loses every slot
  silently — `ensure_data_dir()` reports that at startup. A bundled `.app` runs
  with the working directory set to `/`, so a relative fallback always fails.
- **Menu-bar icon** uses `icon_as_template(true)` with the black artwork. macOS
  tints it for the current appearance, including the inverted state while the
  menu is open. **No polling theme watcher is needed** — unlike the Windows
  `spawn_theme_watcher`. `tray-white.png` is unused on macOS.
- **`set_tooltip` is a no-op on macOS** — `NSStatusItem` has no tooltip, so
  "CQ Paster — *folder*" never appears. The `Folder: <name>` submenu label still
  answers "which folder am I in?".
- **`ActivationPolicy::Accessory`** makes it a menu-bar app with no Dock icon.
  Without it Tauri registers as a regular foreground app. Setting `LSUIElement`
  in `Info.plist` would avoid a possible Dock flash at launch; not done.
- **Native title bar**: `Titled | Closable | Miniaturizable | Resizable |
  FullSizeContentView`, transparent title bar, hidden title, zoom button hidden.
  The window background is painted the same charcoal as the bar because
  `FullSizeContentView` exposes it along the top edge.
- **A 1px hairline remains along the top edge in light mode.** It is drawn by
  the window frame, whose colour follows the window's *appearance*. Forcing a
  dark appearance removes it, but the web view inherits that and the whole UI
  pins to the dark theme. Following the system setting was judged worth more.

### 7.2 The popup

> **Original prediction:** the popup needs to be an `NSPanel` with
> `.nonactivatingPanel` so it never steals focus.
>
> **Reality: not needed.** Tauri's `"focus": false` already stops it becoming
> key — verified by pasting into a focused text field with the popup up and
> watching the caret keep blinking. No `NSPanel` subclass exists in the port.

What macOS *does* need is collection behaviour and ordering:
`CanJoinAllSpaces | FullScreenAuxiliary | IgnoresCycle`,
`NSPopUpMenuWindowLevel`, and `orderFrontRegardless` (an Accessory app is never
active, so plain `orderFront:` can be dropped).

The popup stays up while Cmd is held and follows the cursor at 30 Hz, polled on
a worker thread — **not** by tapping mouse-moved events, which would violate
§5.1. A 10 s cap remains because the Cmd release can be missed during injection.

**Known limitation:** the popup still does not draw over another app's
full-screen Space, despite the above. Parked, not solved.

### 7.3 Autostart

`tauri-plugin-autostart` with `MacosLauncher::LaunchAgent` works unchanged. But
the first **release** launch enables it and writes a marker, baking **whatever
path the app is at** into the login item. Install to `/Applications` *before*
first launch, or the login item points at `~/Downloads` or a build directory
forever.

### 7.4 CQ Jotter

A notepad per folder, sharing the control panel with Paster. **macOS only for
now**: `jotter.rs` and its two commands are behind `#[cfg(target_os = "macos")]`,
and the frontend only calls into `jotter.ts` when `IS_MAC`. The Windows control
panel's HTML was checked to be unchanged by rendering the old and new `main.ts`
side by side with a Windows user agent, across the folder menu's states.

**Storage is JSON on purpose.** `jotter.json`, beside `folders.bin`, is written
to a temporary file, flushed and renamed over the old one. bincode would turn any
future field into silent data loss, and a note is the one thing in the app that
can't be recaptured by copying it again. A file that won't parse is renamed
`jotter.unreadable-<secs>.json`, never overwritten; one that can't be read at all
makes the frontend refuse to save for the session.

**The model is a flat list of lines with depths** (`outline.ts`), not a tree — a
line's sub-lines are simply the deeper lines after it. Every edit is a pure
function returning new arrays, so an undo snapshot is just a reference.
`normalize` repairs indentation after lines are joined without turning siblings
into a staircase.

**The editor.** One contenteditable with a div per line. WebKit does the typing
inside a line, so accents, dictation and spelling corrections stay native.
Anything structural is intercepted (`beforeinput`, `keydown`, `paste`, `copy`,
`cut`), applied to the model and redrawn: Return, Tab, Backspace or Delete at a
line's edge, and any delete or insert that spans lines. The dots sit in a layer
*over* the text rather than in the editable content, so the caret and selection
can never land on one. Their positions come from each line's `offsetTop`, and the
geometry constants in `jotter.ts` must match `.j-line` in `styles.css`.

**Undo is the Jotter's own.** Intercepted edits never reach WebKit's undo stack,
so ⌘Z is handled in `keydown`, with `historyUndo` as a fallback. Keystrokes on
one line with no pause over 1.5 s undo as one step.

**Jotpads.** On screen, Jotter's folders are *jotpads*, with a notepad icon; the
code and `jotter.json` still say `folders`. The folder menu is shared with Paster
and takes its noun and icon from its `FolderSource`, so Paster's HTML doesn't
change. On macOS the footer of both views reads "You can close this window. cQ
runs in the background."; Windows keeps its wording.

**Clear Jots** removes only the crossed-out lines (`removeCrossed`). Its Undo,
`restoreCrossed`, puts each removed run back after the line it followed. Those
lines are found again in the note as it is now by text and indent (a longest
common subsequence), so anything typed or edited during the 10 seconds is kept.

**Crossing out is inherited.** A line reads as crossed out if it or any line it
sits under is marked done. A sub-line of a crossed group can't be toggled on its
own, and bringing the parent back restores each sub-line's own state.

**Window height.** Paster sizes the window to fit its slots; Jotter never resizes
it. `fitMainWindow` is gated off in Jotter view, and the height at switch time —
or after a manual resize — is saved, so a launch straight into Jotter opens at
that height.

**Redraws.** `state-updated` keeps arriving while Jotter is showing, on every
chord. Rebuilding the page then would take the note out from under the caret, so
Jotter view only stores the state, and patches its folder menu and footer in
place instead of re-rendering.

**How it was verified.** The model with Node unit tests; the editor in a real
`WKWebView` — the engine the app uses, not a browser — driven by native `NSEvent`
keys and clicks through AppKit, with Tauri's IPC mocked (88 checks, across a
simulated relaunch). Neither script is in the repo. Two traps in that setup,
should it be rebuilt: an inactive app's window swallows the first click unless
the web view accepts first mouse, and WebKit re-sends unhandled ⌘ shortcuts
through `NSApp.keyWindow`, which is `nil` in an app that was never activated — so
⌘A can't be exercised that way.

### 7.5 Jotter reminders

Per folder: on or off, every N minutes, a window of hours and days, and a sound.
The settings are saved with the folder in `jotter.json`; `customEvery` and
`hours` exist only for the menu.

**The schedule runs in Rust, not in the web view.** The control panel is hidden
most of the time, and a hidden web view's timers are throttled. `jotter_save`
hands every saved document to `reminders::update_doc`, which parses only the
folders, lines and settings (unknown fields ignored; a missing `reminder` means
off). A thread checks every 15 s.

**The rules**, all pure functions with tests:
- Times are multiples of `every` counted from midnight, so hourly lands on the
  hour. Both ends of the window count. A window with `start > end` runs past
  midnight and belongs to the day it opened.
- Each time fires once. A time found more than 90 s late is skipped: the Mac slept.
- A folder with nothing open stays quiet — the same crossing-out rule as
  `outline.ts`.
- A snooze (10 min) holds the regular schedule until it fires.
- Switching reminders on starts with the next time, not the one just past.

**The card** is a second window, `reminder`, created in code on macOS only rather
than declared in `tauri.conf.json`, so Windows gets no hidden extra web view. It
reuses the popup's float and order-front helpers, plus `accept_first_mouse`, so a
click presses the button instead of only activating the window. The backend
emits the card; the window draws it, measures it without waiting for a frame (a
hidden window may never get one), and calls `reminder_present` with its height,
which places it top-right of the screen under the pointer. It needs its own
capability, or it can't receive events at all. Crossing lines out
while a card is up updates it, and crossing out the last one takes it down.

**Place windows in AppKit points, not Tauri positions.** The card first used
Tauri's `cursor_position`, `monitor_from_point`, `work_area` and `set_position`.
With a 2x laptop and 1x external displays those disagree: the pointer is scaled
by the primary screen's factor, each monitor's area by its own, and a window
move by the factor of whichever screen the window was last on. The pointer
matched no monitor, the code fell back to the laptop, and the card landed
mid-screen on an external display. `place_card` now reads `NSEvent.mouseLocation`
and `NSScreen` frames and sets the `NSWindow` frame directly — one coordinate
space for every screen. A unit test holds that three-screen layout.

**The menu-bar dot** can't be part of the icon, because template images are drawn
in a single colour. It's an `NSBox` laid over the status item's button, reached
through `with_inner_tray_icon` — which blocks on the main thread, so, like
`refresh_tray`, it always runs from a spawned thread.

**Focus.** Clicking the card activates CQ. Dismiss and Snooze call
`NSApplication.deactivate()` when the control panel isn't showing, to hand focus
back to the app the user was in. Not yet confirmed in the installed app.

**Same limitation as the popup:** the card doesn't draw over another app's
full-screen Space.

**Dismiss and Snooze don't bring the control panel forward.** Clicking the card
makes CQ the active app, and when an active app's key window goes, macOS makes its
next window key and brings it to the front: the control panel, if it's open behind
other windows. `put_away` checks the on-screen window list (`CGWindowListCopyWindowInfo`)
first. If the panel is the frontmost ordinary window, the user was in it and stays
there. Otherwise CQ deactivates, and the card is hidden 150 ms later. The card
is also ordered in directly rather than with `show()`, which would make it the key
window and take the keyboard from the panel mid-typing. Found with `log_windows`,
which records the panel's state and the frontmost app when a card is shown, when
Dismiss or Snooze is pressed, and a second later; remove it once this is confirmed.

### 7.6 CQ Shotter

Screenshots taken while CQ is running, newest first, as a third tool beside
Paster and Jotter. **macOS only**, like Jotter.

**The switch moved into the title bar.** Three labelled tools don't fit in the
header beside each tool's own controls, so on macOS `titlebar()` draws the switch
where the window's name was. It reuses `.mode`, so the colours cross-fade as
before; Shotter's blue is the same gradient recipe as the green and the orange.
The bar stays a drag region: only the bar itself starts a drag, not its buttons.
Windows keeps its title and its Master/Noob switch; its HTML was checked unchanged.

**Finding screenshots** (`shotter.rs`). A thread reads the Screenshot app's folder
(`com.apple.screencapture location` through `CFPreferences`, else the Desktop)
once a second. The first successful read only notes what is already there. A
file that appears later counts if it carries `com.apple.metadata:
kMDItemIsScreenCapture`, which macOS sets on its own screenshots, rechecked for up
to 5 s in case the tag lands after the file. Hidden files are skipped: macOS
writes a dot-file first. No FSEvents and no new crates; the hook is untouched.

- **Measured:** on the author's Mac, the file lands 3–7 s after the time in its
  name, because of the floating thumbnail. Nothing CQ does can make it sooner.
- **Desktop access** is a privacy-protected folder: the first read prompts, and
  reads fail until it's answered.

**The list** is `shotter.json` (ids, paths, times), written with `jotter::save`'s
atomic write. Entries whose file is gone drop off on every scan.

**Renaming** (`shot_rename`) moves the file where it sits, so the screenshot tag,
its creation date and its place in the list all survive — a rename keeps the same
file, unlike the copy-and-replace that saving markup does. The typed name is put
through `clean_name`: no `/` (illegal) or `:` (Finder shows it as `/`), no leading
dot (it would hide the file from Finder and from the scan), no trailing dots, and
capped at 200 bytes, since the file system's 255 is in bytes and a name in a
non-Latin script reaches it sooner. The extension is the original's, whatever was
typed. A name another file already has is refused: replacing a file that may not
even be a screenshot isn't the app's call. The list is updated under the scan's
own lock, and the new path is marked as seen, so the scan can't come across the
renamed file first and list it as a second screenshot.

**Copy** writes the file's bytes as one pasteboard item of its type, through
`clipboard::restore`. **Trash** (one screenshot, or all of them for Clear Shots)
uses `NSFileManager.trashItemAtURL` and remembers where each file landed. Undo
`rename`s them back, holding the list's lock so the scan thread can't list a
returning file a second time, and re-sorts newest first.

**Saving markup over the original** keeps what makes it a screenshot. The PNG
from the canvas is written to a hidden file beside the original. It gets:
- the original's extended attributes, including the screen-capture tag, which a
  plain rename would lose, taking the file out of Shotter and Spotlight
- the original's `pHYs` chunk, copied byte for byte, since a chunk's CRC doesn't
  depend on its position, so a 144 dpi Retina screenshot still pastes at its size
- the original's permissions

It's then flushed and renamed over the original. The image travels as a raw IPC
body (`tauri::ipc::Request`) with the id in a header, not as a JSON array of
numbers, so a 5K screenshot doesn't become megabytes of text.

**The markup window** is created in code the first time, like the reminder card.
- **Title bar:** the main window's native title bar (`make_native_titlebar` now
  takes a label).
- **Size:** it fits the image within 85% of the screen under the pointer, in
  AppKit points (§7.5). A `pHYs` of 144 dpi halves the image's size in points.
- **Marks** are stored in image pixels, with the stroke width converted from
  points when the stroke starts. Done redraws them on a full-size canvas.
- **Closing** hides the window for next time.
- **Loading an image:** `markup_current` covers a window that loads after the
  `markup-open` event was sent.

**How it was verified.**
- **Rust unit tests:** baseline, tag and grace, pruning, folder setting, points
  from `pHYs`, saving keeps tags and density, window size.
- **Node:** geometry and time labels.
- **WKWebView harness**, with the backend mocked:
  - the tabs, including at the 380 px minimum width
  - the list, copy, trash and Undo, and relaunch
  - drawing with native mouse events, checked in the canvas's pixels, and a
    saved PNG decoded at full size

The harness can't hover a background window, so the card's buttons were checked
through `:focus-within`, which shares the rule.

### 7.7 Shake to open

Shaking the mouse opens the control panel. **macOS only.**

**The thresholds come from a recording, not a guess.** A standalone recorder
sampled the pointer at 100 Hz through 25 s of deliberate shaking and 60 s of
ordinary work, and the detector was then tuned by replaying that file. What it
showed:

| | Turns inside 0.6 s | Peak speed |
|---|---|---|
| A deliberate shake | 9–10 | 9,000–15,000 pt/s |
| The busiest second of ordinary use | 2 | — |

The gap is wide enough that the exact numbers hardly matter: every speed between
900 and 3200 pt/s gave the same answer. Two slices of that recording are the
tests (`src/testdata/*.trace`): the real shake has to fire, the real ordinary use
must not, at every sensitivity.

**A shake is measured in any direction.** The first attempt counted sideways
reversals only and missed a shake that was more diagonal than horizontal. A turn
is now two fast strokes more than 135° apart, so orientation doesn't matter —
which is also how macOS's own shake-to-locate behaves.

**Its own tap, never the keyboard's.** A mouse can deliver a thousand events a
second, and the keyboard tap is the one that suppresses the chord keys: if that
one ever runs slow, macOS disables it and the hotkeys die (§5.1). This tap is
listen-only, on its own thread, created with its own port. Switching the gesture
off calls `CGEventTapEnable(port, false)`, so the callback isn't merely skipped —
no events are delivered at all. Settings are read from atomics, never a lock.

**Dragging is not shaking:** any drag event clears the detector, so throwing a
window around can't open anything.

**A trap worth remembering:** `last_fire` started at zero, which put the 2 s
cooldown over the first two seconds after the tap started — a shake right after
launch did nothing. It starts at negative infinity instead. The tests caught it
because they replay from t = 0.

Settings live in `shake.json` beside the others, and the menu-bar menu carries the
switch and the three sensitivities.

### 7.8 Dictation

Hold the right `⌥` key, speak, let go: the audio is transcribed on the machine
and pasted at the cursor. **macOS only.**

**Right Option, because nothing else was available.** The spike that chose it
logged every `flagsChanged` event the tap saw, behind a marker file, and the
answer was decided by what the keyboard actually produced:

| Key | Keycode | Result |
|---|---|---|
| Right `⌥` | `0x3D` | seen, and claimed by nothing |
| Right `⌘` | `0x36` | seen, but `is_command_key` already matches it |
| Right `⇧` | `0x3C` | seen, but a held Shift is worse to live with |
| Right `⌃` | `0x3E` | **never arrives** — Apple laptops have no such key |
| fn / 🌐 | `0x3F` | **never arrives** — consumed below a session tap |

Seeing fn would mean moving CQ's tap to `kCGHIDEventTap`, changing the
foundation of every existing hotkey to gain one key. The trigger reads its side
from the flag word's device bit (`0x40`) rather than the keycode: right Option
down is `0x00080140`, up is `0x00000100`. It is passed through, never swallowed
— Option is a real modifier and holding it must still reach the app underneath.

**The microphone opens on the press, and the threshold is applied on release.**
Waiting to learn whether a hold was deliberate would clip the first word. 400 ms
comes from the same recording: deliberate taps measured 127–282 ms, deliberate
holds 2462–4727 ms.

**The engine is a bundled executable, not a library.** `whisper-server` ships in
`Contents/MacOS/` as an `externalBin` and is spoken to over loopback HTTP on a
port the OS picks. That keeps C++ out of the Cargo build — Windows CI never sees
it — isolates an inference crash from the clipboard manager it lives in, and
lets the 0.8 GB the model occupies be released by ending a process. It is built
by `scripts/build-whisper-server.sh` and is **not in git**: ~20 MB, and
rebuilding whisper would add another copy to history each time.

**`externalBin` is validated while compiling, not while bundling.** Declaring
it in the shared `tauri.conf.json` broke both CI jobs — macOS could not find
the binary, and Windows went looking for a `whisper-server-x86_64-pc-windows-
msvc.exe` that will never exist. It belongs in `tauri.macos.conf.json`, which
Tauri merges only for macOS targets, so Windows never sees it at all. The same
applies to `Entitlements.plist`. CI puts a placeholder in place on macOS: it
never bundles or signs, so the file only has to exist for `cargo build` to
reach the tests.

`GGML_NATIVE=OFF` is required in that script. Left on, ggml detects the host CPU
and passes `-mcpu=apple-m4` into the x86_64 half of the universal build, which
fails with *unknown target CPU*.

**Two decoder flags are not adjustable, and both cost words when wrong.**

| Flag | Effect if wrong |
|---|---|
| `-nt` (no timestamps) | **Never passed.** It dropped a whole sentence and garbled another on a 99-second recording — 155 words against 164 — identically every run. Timestamp tokens are part of how the decoder tracks position, and the returned text has no timestamps in it anyway. |
| `-bs 5` (beam search) | **Always passed.** Greedy decoding dropped a sentence from the same recording. |

Both are asserted in tests with the measurement in the failure message.

**Nothing is resampled before the engine.** whisper.cpp decodes with miniaudio as
`ma_decoder_config_init(ma_format_f32, channels, WHISPER_SAMPLE_RATE)`, which
converts rate, format and channels itself; verified by sending it 48 kHz stereo
— what a Mac microphone offers — and getting the same transcript as the 16 kHz
mono original. A hand-rolled decimator would only be worse.

**The vocabulary is worth more than spelling.** Whisper's initial prompt primes
the decoder before it hears anything. Ten terms turned "created underscore at"
into `created_at`, "customer ID" into `customer_id`, "3.30" into "3:30", and put
real quotation marks around a quoted sentence — none of it asked for; the model
infers the register from the words it is primed with. That is four of the seven
jobs a local rewrite model was going to be needed for.

It nearly got thrown away: the prompt took the transcript from 164 words to 152,
which looks exactly like the `-nt` failure. It is the opposite — the words were
compressed, not deleted, and the twelve are accounted for by five terms losing
their spoken "underscore". **A word count rejects this improvement; only the
diff shows what it is.**

The budget is small and overflows silently: the prompt is capped at
`min(n_max_text_ctx, n_text_ctx/2)` = 223 tokens, past which whisper keeps only
the **last** 223 and says so in a log. Measured against the real model, 60 terms
(589 chars) fit and 80 (786 chars) came to 298 tokens and were cut. The list is
therefore capped at 48 in `vocab.rs`, where the window can show the count and
turn red, rather than being trimmed where the loss is invisible.
`carry_initial_prompt` is set, or the vocabulary primes only the first
thirty-second window.

**Timing, measured on an M4 Pro:** 0.4 s to start the engine warm, ~14.5 s the
first time after a boot when 547 MB comes off disk, 0.6 s to transcribe five
seconds of speech, 0.8 GB resident while loaded. The cold start is a page-cache
effect, not engine overhead — the socket only opens once the model is loaded, so
it is a valid readiness signal.

**The listening mark must never become key.** `show()` goes through
`makeKeyAndOrderFront:`, which activates CQ and takes focus off the app being
dictated into — the Dismiss bug in §7.5 again. It is ordered in with
`orderFrontRegardless` and made click-through with `setIgnoresMouseEvents:`. It
follows the pointer at the popup's rate with the same generation counter, and
does not move while the mouse is still, since `set_position` is marshalled to
the main thread.

**A trap worth remembering:** the mark never appeared at all in its first
version, and the beachball said why. A synchronous Tauri command runs on the
main thread, and the command driving it slept for the length of the recording;
showing the mark queues `orderFrontRegardless` onto that same thread, so it only
got its turn after the recording had been put away. Anything that sleeps belongs
on a thread of its own with the result delivered as an event.

**Pasting borrows the pasteboard and hands it back**, exactly as a chord paste
does, and only if nothing else claimed it meanwhile. It raises the tap's
`injecting` guard, which now has a handle outside the worker because
transcription cannot be done on the worker thread.

**Every dictation is recorded** in `dictations.log`: what was heard, what the
clean-up made of it, which was pasted, and why if the guard refused. One JSON
line each, capped at 512 KB with one previous generation. Written by Rust —
the first version sent the transcript to the control panel to append to a
jotpad, which made the record depend on a window being alive to receive it.

It was a jotpad at first, and that was wrong twice over. A list of raw
transcripts is not something anyone wants among their notes; and `repairDoc`
builds every folder from scratch and never read `kind` back from disk, so each
launch found no dictation pad and made another one. Five had accumulated before
it was noticed — the check after installing it looked once, when there was
exactly one. `retireDictationPads` merges them into a single ordinary jotpad,
keeping the text, and nothing creates one again. Verified against the real
file: 5 pads to 1, all 21 dictated lines kept.

The plain-jotpad machinery went with it, which returned `renderLines`,
`layoutDots`, `onDotDown` and `canClear` to being byte-identical to before
dictation existed — a better answer to the Windows question than the reasoning
that had been standing in for it.

**The mark stays up while the words are worked on.** It used to vanish when the
key came up, which is exactly when there is something to wait for. It keeps
following the pointer, becomes a soft glow in the three tool colours, and comes
down just before the paste — not after, since the paste puts text where the
user is looking and a spinner still sitting there would be the first thing they
saw instead of their words. One event with a boolean drives it: two events
would allow switching into the working state with nothing to switch back,
which is what the first attempt did.

**The window is much larger than what it draws, and has to be.** The glow
reaches about 17 points past the shape, and a window sized to the shape clips
that halo off square — a hard edge on a soft glow is exactly what gives away
that there is a window there. 84 x 66 points around a 34 x 26 blob, the rest
transparent and click-through. The offset places what is *drawn* beside the
pointer, not the window, which is mostly empty margin.

### Cleaning up what was said

A second sidecar, `llama-server`, rewrites the transcript before it is pasted;
right `⌥` + `Shift` skips it. It makes two edits and no others — a spoken
self-correction keeps only the corrected version, and filler goes. Everything
else is the vocabulary prompt's work already, and asking twice would only add
risk.

**Qwen2.5-7B-Instruct Q4_K_M**, Apache 2.0, 4.7 GB, ~6 GB resident. The 3B was
tried and rejected: 5/8 against the 7B's 8/8 on the same corpus, emitting
invalid bytes and deleting words, and non-commercially licensed besides.

**The cost is not what the first spike suggested.** That measured a 99-second
dictation — 555 tokens in, 222 out. A real dictation is a sentence, and
generation is proportional to output: ten tokens is 208 ms. Measured on three
dictations read aloud: 427–882 ms each, all three correct.

`cache_prompt` is on, so the 160-token instruction is evaluated once and is 17
tokens thereafter — 649 ms for the first rewrite after launch, 244–400 ms
after. The engine is started and the instruction pre-warmed when the trigger
goes **down**, so both happen while the user is still speaking. Generation
cannot be overlapped: a correction is only knowable once the sentence ends.

**The guard, and why a single threshold was wrong.** The instruction forbids
rewording, so the model can only delete, which makes the output checkable. The
first version rejected any word that was not in the transcript and required
half the words to survive. Both failed on real speech:

| Seen | Why the first guard was wrong |
|---|---|
| `Invented("the")` | A joining word cannot change what a sentence says. Removing a correction sometimes needs one back. |
| `TooShort { kept: 5, had: 18 }` | Two false starts, and the model's answer — "Let's do Tuesday at 2:30." — was right. The guard destroyed it and pasted the stumbling version. |

So: joining words may be added, and the size floor depends on whether the
speaker corrected themselves — 20% with a correction marker present, 70%
without, since without one nothing should be leaving but filler. Both cases are
fixtures.

**A word count cannot see a lost sentence.** Lose one of six and 83% of the
words still survive, above any floor worth setting; and once a correction is
present the floor has to be permissive enough for a double false start, so it
goes blind entirely. Each sentence of the transcript is therefore checked on
its own: it must leave a trace in the output, judged by the words unique to it,
because a word repeated elsewhere proves nothing about whether *this* sentence
survived. A sentence may vanish only when it, or the one after it, carries a
correction marker — which is what being superseded looks like.

**The failure this was written for could not be reproduced.** Seventeen
dictations were tried: short ones with corrections, realistic 100–137 word
ones, and eight built around the exact structure of the original loss — a
sentence immediately after a correction, including the sentence that failed
before. None dropped anything; 95–98% of words survived throughout.

That says the blind spot was mis-attributed rather than fixed by luck. The
original loss happened under the earlier, formatting-heavy instruction that
turned speech into bullet lists and code fences; a model rearranging text drops
pieces. The shipped instruction only deletes, and measures that way. **The
narrow instruction is the mitigation; the guard is the second line.**

The check was then run against all 28 real rewrites gathered while building
this — every provocation plus the dictations read aloud for testing — and
rejected none of them. That number matters because the previous guard rejected
a third of real output.

**Idle.** The engine exits after ten unused minutes and gives back its memory.
The reload starts on key-down, but that only hides it when the model file is
still in the page cache — 1.1 s against 17.5 s cold. A dictation therefore
waits four seconds and then pastes as heard: a 17-second pause mid-sentence is
worse than an untidied sentence.

`Info.plist` carries `NSMicrophoneUsageDescription` and `Entitlements.plist`
carries `com.apple.security.device.audio-input`. Both halves are needed: the
string is what macOS shows, the entitlement is what the hardened runtime
requires before the access is allowed at all.

---

## 8. Building and signing

### Prerequisites

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

### Dev

```bash
npm install
npm run tauri dev
```

`CQ_DEBUG=1` traces the whole chord pipeline to stderr — arming, copy, paste,
what was captured with every UTI and byte size, and the pasteboard handback. It
is **off by default**, worker-thread only, and **logs clipboard text**, so it is
a debugging aid and not something to leave enabled.

### The signing certificate

The certificate is a local keychain identity, not in the repo. To recreate:

```bash
openssl req -x509 -newkey rsa:2048 -keyout k.pem -out c.pem -days 3650 \
  -nodes -subj "/CN=CQ Paster Self Signed/O=CQ Paster/C=US" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning"
openssl pkcs12 -export -out c.p12 -inkey k.pem -in c.pem -passout pass:PW
security import c.p12 -k ~/Library/Keychains/login.keychain-db \
  -T /usr/bin/codesign -P PW
```

`codesign` accepts it without the certificate being trusted, so no trust-store
change and no admin rights are needed. The first build prompts once for keychain
access — choose **Always Allow**. `bundle.macOS.signingIdentity` in
`tauri.conf.json` points at it, and is ignored on Windows.

A build on another Mac needs a certificate of the same name, or the config
changed. Without it the build silently reverts to ad-hoc signing and
reintroduces §5.12.

### Release build

```bash
npm run tauri build -- --target universal-apple-darwin
```

Outputs a universal (`x86_64 arm64`) `.app` and `.dmg`. Verify with
`lipo -archs` rather than trusting the target name.

### Gatekeeper

Self-signing does **nothing** for other people's Macs — the certificate is not
trusted, so recipients still need right-click → **Open**, or:

```bash
xattr -dr com.apple.quarantine "/Applications/CQ Paster.app"
```

Proper distribution needs an **Apple Developer ID** ($99/yr) plus notarization.
Self-signing solves permission stability, not distribution.

### Publishing a release

**One release, both platforms' assets** — do not create a competing release
scheme. Attach the `.dmg` to the release for the version it was built against,
alongside the Windows `.exe`, matching the Windows naming convention:

```
CQ_Paster_0.5.1_x64-setup.exe      <- Windows asset
CQ_Paster_0.5.1_universal.dmg      <- macOS asset
```

Tauri emits `CQ Paster_<version>_universal.dmg` with spaces; rename it to match.

Two places must be updated when the Mac build first ships, both of which say
"in development" until then:

1. **`README.md`** — the *Downloads* table at the top, and the *macOS (in
   development)* section heading. The "How the two versions will differ" table
   already written there should stay.
2. **The GitHub release notes** — replace the *macOS — coming soon* section with
   real install instructions, including the Gatekeeper workaround and the two
   permissions the user must grant.

**Ask before publishing.** Releases are public and the user drives timing.

### Resetting permissions during development

Only needed when the signing identity changes:

```bash
tccutil reset Accessibility com.cqpaster.app
tccutil reset ListenEvent com.cqpaster.app
```

---

## 9. Test matrix

Verified on macOS unless noted. Windows passes all of these.

**Chords**
- [x] `Cmd+1+C` … `Cmd+9+C` store into the right slot
- [x] `Cmd+N+V` pastes the right slot
- [x] `Cmd+Shift+N+V` pastes stripped of formatting (tested in TextEdit)
- [x] Plain `Cmd+C` / `Cmd+V` completely unaffected
- [x] Plain `Cmd+C` after a chord does **not** clobber a stored slot (§5.2)
- [x] Plain `Cmd+V` after a chord pastes the user's own clipboard (§4.6)
- [x] `Cmd+1`…`Cmd+9` swallowed — browser tabs don't switch

**Content types**
- [x] Plain text
- [x] Rich text / HTML — including WebKit apps (§3.2)
- [x] Images — automated live test asserts IHDR dimensions and a byte-identical round trip
- [x] Files in Finder — pastes as a **copy**
- [x] Multiple files at once — three items, three distinct paths
- [ ] Password manager content skipped — **partially deliverable only**, see §3.6
- [ ] A **slow/large** copy captures the new content, not the previous clipboard
- [ ] A chord fired with **nothing selected** leaves the slot untouched rather
      than overwriting it with stale content

**Folders**
- [x] Slots are independent per folder
- [x] Create switches to the new folder
- [x] Folders survive a restart
- [ ] Clear all / Undo scoped to the active folder — unit-tested, not re-checked by hand
- [ ] Menu-bar folder submenu switches folders

**Windows/UI**
- [x] Cursor popup appears near the cursor and never steals focus
- [x] Popup stays while Cmd is held and follows the cursor
- [x] Menu bar icon looks right in light and dark
- [ ] Popup over a full-screen app — **known limitation**, §7.2
- [ ] Start-on-login verified end to end

**Jotter** (§7.4)
- [x] The switch flips the content, keeps the window height, and the title follows
- [x] Typing, Return, Tab / Shift+Tab, Backspace at line edges, deleting and
      typing across lines — native WebKit input
- [x] Crossing out a line crosses out its group; sub-lines of a crossed group are inert
- [x] ⌘Z / ⇧⌘Z step through edits one at a time
- [x] Clear Jots removes only crossed-out lines; Undo puts them back in place,
      keeping anything typed after the clear
- [x] Jotpads: create, switch (caret restored), delete; counts show open lines
- [x] Notes, folders, last side and height survive a relaunch
- [x] A Paster update mid-typing leaves the note and the caret alone
- [ ] In the installed app: ⌘A, ⌘+N+V into a note, press-and-hold accents and IME

**Jotter reminders** (§7.5)
- [x] Schedule rules — unit tests: clock alignment, window ends, overnight
      windows, days, once per time, skipped after sleep, snooze, nothing open
- [x] Settings menu: on/off, presets and custom, working or custom hours and days,
      sound with preview, independent per folder — WKWebView harness
- [x] Card: open lines per folder, the rest counted, names escaped, sizes itself,
      buttons reach the backend — WKWebView harness
- [x] The card lands top-right of the screen under the pointer — checked on a
      2x laptop with two 1x displays, one of them portrait
- [ ] In the installed app: sound, menu-bar dot, focus after Dismiss or Snooze,
      and a real scheduled reminder firing
- [ ] Dismiss and Snooze leave the control panel where it was, and don't take the
      keyboard from it when the card appears — fixed, to confirm (§7.5)

**CQ Shotter** (§7.6)
- [x] Title-bar switch: three tools, centred, clear of the traffic lights at
      460 and 380 px; the last tool reopens at its height — WKWebView harness
- [x] Watching: files already there ignored, tagged new files added newest
      first, late tags, untagged and hidden files ignored, deleted files dropped
      — unit tests
- [x] List, copy, trash + Undo, Clear Shots + Undo, empty state, dark mode —
      WKWebView harness
- [x] Renaming: click-to-edit, Enter, Escape, a taken name refused, a screenshot
      arriving mid-word leaves the field alone — WKWebView harness; the file
      moves and keeps its tag, and the scan doesn't re-list it — unit tests
- [x] Markup: pen, arrow, colours, sizes, Undo, Esc/Cancel; Done sends a
      full-size PNG with the marks in it — WKWebView harness
- [x] Saving over the original keeps the screen-capture tags and pixel density
      — unit tests
- [ ] In the installed app: the Desktop-access prompt, a real screenshot
      appearing, copy and paste, renaming, Trash, Clear Shots and Undo, markup saved and still tagged
      (`mdls -name kMDItemIsScreenCapture`)

**Folder arrows** (§4.8)
- [x] Stepping wraps both ways and does nothing with one folder — unit tests
- [x] Popup: chevrons only with 2+ folders, a long name truncates without losing
      them, the hint on its own line, the card fits 300×320 — WKWebView harness
- [ ] In the installed app: `⌘+N` then `←` / `→` switches folders in the popup,
      `C`/`V` then use the new folder, and `⌘←` / `⌘→` still work in apps
      otherwise

**Shake to open** (§7.7)
- [x] A recorded shake fires at every sensitivity; a recorded minute of ordinary
      use fires at none; dragging, a fast straight sweep and a slow wiggle never
      fire; the cooldown holds — unit tests over recorded traces
- [ ] In the installed app: shaking opens the panel, the menu switch and the
      sensitivities take effect, and a day of ordinary work stays quiet

**Regression**
- [x] `cargo test` passes — 66 tests on macOS, plus 4 `#[ignore]`d
      live tests run with `cargo test -- --ignored`
- [x] The **Windows** build still compiles — checked by CI
      (`.github/workflows/ci.yml`), which builds and tests both platforms on
      every pull request. This cannot be checked locally from macOS (no MSVC
      toolchain), so before CI existed the guarantee rested on `cfg` discipline
      alone.

**Dictation (§7.8)**
- [x] Hold right `⌥`, speak, let go — text lands at the cursor
- [x] Hold duration matches what is captured — 1529/1999/5027 ms gave 1.5/2.0/5.0 s
- [x] The model downloads, verifies against its published SHA-256, and the tray
      relabels itself when it lands
- [x] The listening mark appears beside the pointer, follows it, and tracks the
      voice rather than animating on its own
- [x] The vocabulary reaches the decoder — spoken "underscore" comes through as
      one, confirmed on the user's own speech
- [x] Every dictation is recorded in `dictations.log`, both versions and which
      was pasted
- [x] Dictation pads from older versions merge into one ordinary jotpad,
      keeping their text — checked against a real file with five of them
- [x] Other jotpads keep their bullets and crossing off
- [x] The microphone list shows real devices; the chosen one is used and logged
- [ ] **A quick tap of right `⌥` records nothing** — the 400 ms threshold is
      unit-tested against measured hold times, not checked by hand
- [ ] **`⌥` plus a letter still types its special character** while dictation is
      installed — the trigger is passed through, but this was not re-checked
- [ ] **The clipboard survives a dictation** — the borrow-and-hand-back path is
      shared with chord paste but has not been exercised here by hand
- [ ] **A locked microphone that is unplugged** falls back and says so
- [ ] Resume a half-finished model download — the curl path was verified
      standalone, never through the app
- [ ] A dictation longer than one 30-second window, to confirm
      `carry_initial_prompt` keeps the vocabulary alive throughout
- [ ] Dictating into a password field, or any app that refuses a paste
- [x] The rewrite resolves a self-correction, leaves finished text alone, and
      handles two false starts in a row — read aloud, 427–882 ms each
- [x] The guard rejects a rewrite and pastes the transcript instead
- [ ] **A day of ordinary work** with the trigger live, to find out whether
      right `⌥` collides with anything in practice
- [ ] **The idle timeout actually firing**, and the memory coming back. The
      wiring is verified; ten minutes of waiting is not
- [ ] **The spinner on a slow dictation.** It shows while transcribing and
      cleaning; only a dictation long enough to take a second proves it
- [x] **A dictation that both corrects itself and loses a sentence** is caught
      by the per-sentence check, which a word count cannot do. Note that the
      failure itself could not be provoked in 17 attempts — see §7.8
- [x] The guard accepts all 28 real rewrites gathered while building this,
      having rejected a third of them before

---

## 10. Still open

- Re-run the matrix against a **release** build; release timing differs from
  debug and the copy path is timing-sensitive.
- Popup over full-screen Spaces (§7.2).
- `is_sensitive()` is weaker than Windows and cannot reliably skip password
  managers (§3.6).
- `CQ_DEBUG` logs clipboard text; decide before release.
- Stamp the DMG filename with a version or build id (§5.13).
- Developer ID signing and notarization, if the app is ever distributed beyond
  a machine that trusts the self-signed certificate.
- Jotter on Windows. The frontend is shared, so it needs `jotter.rs` un-gated,
  the `IS_MAC` checks around the switch lifted, and a look on a real Windows
  machine. Reminders would need a Windows card window, sound and tray badge.
- Confirm the reminder card hands focus back after Dismiss or Snooze (§7.5).
- **Is corrections-only enough?** The rewrite deliberately may not reword, which
  is what makes its output checkable. If that turns out to be less than expected
  — grammar, rambling sentences — loosening the instruction is one line, but the
  guard stops working, because legitimate output would no longer have to come
  from the input.
- **Dictation's vocabulary could gather itself.** The terms are already in CQ —
  in the clipboard slots and the jotpads — so candidates could be harvested and
  offered for approval rather than typed. The 48-term budget means any such list
  must rank and evict, not accumulate.
- **`whisper-server` is bundled but unversioned.** Nothing records which
  whisper.cpp commit produced the binary in `src-tauri/binaries/`.
- **The vocabulary could gather itself** from what CQ already holds — the
  clipboard slots and the jotpads — rather than being typed. Still open.
