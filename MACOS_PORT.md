# CQ Paster — macOS port

Originally a handoff brief written from the Windows side, before any macOS work
existed. **The port is now built and working**, so this has been revised into a
record of how macOS actually behaves — including the places the original
predictions were wrong, which are flagged as they come up. Those corrections are
the most useful part of this document.

Status: macOS works end to end on a signed, installed build. Windows v0.5.0 is
unchanged and still shipping.

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

**Two modes:** *Master* (zero UI) and *Noob* (a reference popup near the cursor).

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
| `src/main.ts` | Frontend for both windows; `MOD` renders `Ctrl` or `⌘` |
| `src/styles.css` | Theme tokens; macOS-specific rules scoped to `[data-platform="macos"]` |

**Gating rule:** everything macOS-specific is behind `#[cfg(target_os = "macos")]`
or `[data-platform="macos"]`. Items that used to be shared and are now
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

### 4.6 Give the clipboard back

Pasting a slot works by writing it to the system pasteboard and injecting
`Cmd+V`. Nothing restores what was there before unless you do it, so slot N
stays on the clipboard and the user's next plain `Cmd+V` re-pastes it. The two
pairs must stay independent.

The pasteboard is snapshotted before being borrowed and handed back after.

**This reintroduces what §5.5 records as removed on Windows** — reading a lazy
provider can make the source app re-assert and clobber. macOS has the same
mechanism, and §3.2 proves those providers are real and load-bearing here. Treat
intermittent wrong-content pastes as this until proven otherwise.

The handback is the **one remaining real delay** (180 ms): macOS offers no
"paste completed" signal. Too short and the target pastes the handed-back
content; too long and a fast plain `Cmd+V` beats it.

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
  while the user is typing a folder name**.
- `set_mode` deliberately **emits no event** so the mode toggle can cross-fade.

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

**Folders**
- [x] Slots are independent per folder
- [x] Create switches to the new folder
- [x] Folders survive a restart
- [ ] Clear all / Undo scoped to the active folder — unit-tested, not re-checked by hand
- [ ] Menu-bar folder submenu switches folders

**Windows/UI**
- [x] Noob popup appears near the cursor and never steals focus
- [x] Popup stays while Cmd is held and follows the cursor
- [x] Menu bar icon looks right in light and dark
- [ ] Popup over a full-screen app — **known limitation**, §7.2
- [ ] Start-on-login verified end to end

**Regression**
- [x] `cargo test` passes — 27 tests (11 original + 16 macOS), plus 4 `#[ignore]`d
      live tests run with `cargo test -- --ignored`
- [ ] The **Windows** build still compiles — *cannot be checked from macOS*; no
      MSVC toolchain. Held by `cfg` discipline alone. A CI matrix building both
      targets would close this gap.

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
