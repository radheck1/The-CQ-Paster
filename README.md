# CQ Paster

[![CI](https://github.com/radheck1/The-CQ-Paster/actions/workflows/ci.yml/badge.svg)](https://github.com/radheck1/The-CQ-Paster/actions/workflows/ci.yml)

An ultra-minimal, hotkey-driven **multi-slot clipboard manager**. Copy up to
**9 things** into numbered slots and paste them back in any order — without ever
leaving the keyboard. Group slots into **folders**, each with its own
independent set of 9.

---

## Downloads

| Platform | Status | Latest | Install |
| --- | --- | --- | --- |
| **Windows** (x64) | ✅ Available | **v0.5.2** | [Download the installer](https://github.com/radheck1/The-CQ-Paster/releases/latest) |
| **macOS** (Apple Silicon + Intel) | ✅ Available | **v0.5.2** | [Download the installer](https://github.com/radheck1/The-CQ-Paster/releases/latest) |

> **Windows:** the build is not code-signed, so SmartScreen shows an "unknown
> publisher" prompt on first run. Click **More info → Run anyway**.
>
> **macOS:** the build is not notarized, so Gatekeeper blocks a normal
> double-click. **Right-click the app → Open → Open**, or run
> `xattr -dr com.apple.quarantine "/Applications/CQ Paster.app"`. You must also
> grant **Accessibility** and **Input Monitoring** — the app walks you through
> both on first launch. See [macOS](#macos) below.

---

## Hotkeys

| Action | Windows | macOS |
| --- | --- | --- |
| Copy selection into slot *N* | `Ctrl` + `<N>` + `C` | `⌘` + `<N>` + `C` |
| Paste slot *N* | `Ctrl` + `<N>` + `V` | `⌘` + `<N>` + `V` |
| Paste slot *N* as plain text | `Ctrl` + `Shift` + `<N>` + `V` | `⌘` + `Shift` + `<N>` + `V` |
| Normal copy / paste | `Ctrl+C` / `Ctrl+V` (unchanged) | `⌘C` / `⌘V` (unchanged) |

`N` is `1`–`9`. **Press the digit before the letter** — hold `Ctrl`, tap `2`,
tap `C` to store the selection in slot 2; later hold `Ctrl`, tap `2`, tap `V` to
paste it. On macOS, hold `⌘` instead.

It works with **anything you can copy** — text, images, files, and app-specific
formats — because each slot stores a byte-exact snapshot of every clipboard
format the source app published, and writes those exact bytes back on paste.

**A slot copy is a stash.** `Ctrl+<N>+C` fills the slot *without* disturbing your
normal clipboard, so a plain `Ctrl+V` still pastes whatever you had before.

### The one tradeoff

While CQ Paster is running, `Ctrl+1`…`Ctrl+9` become its trigger prefix, so those
specific combos no longer reach the foreground app (e.g. browser tab-switching).
Plain number typing is unaffected. The same applies to `⌘1`–`⌘9` on macOS.

---

## Folders

Slots are grouped into folders, and **each folder has its own independent 9
slots**. Pick one from the pill in the top-left of the control panel, or from the
tray's **Folder** submenu.

- Copy, paste, **Clear all** and **Undo** act on the **active folder only**
- Folders never share slots
- Create, rename and delete freely — **Main** is permanent and always present
- Everything persists across restarts

---

## Modes

- **Master** — zero UI. Just you, the hotkeys, and your memory of what's where.
- **Noob** — a small reference popup appears next to your cursor showing all 9
  slots (and the active folder) whenever you start a chord.

Toggle from the tray icon or the control panel. The app lives in the **system
tray**; closing the control-panel window keeps it running.

---

## macOS

Shipping as of **v0.5.2**, as a universal build (Apple Silicon and Intel) in the
same release as the Windows installer.

### Installing

1. Open the `.dmg` and drag **CQ Paster** to Applications. Install it *before*
   first launch — the first run registers start-on-login against wherever the
   app currently is.
2. Right-click the app → **Open** → **Open**. A normal double-click is blocked,
   because the build is signed but not notarized.
3. Grant **Accessibility**, then **Input Monitoring**, when prompted. The app
   opens the right System Settings pane for each and picks them up without a
   restart.

> **Upgrading and the permission list:** if CQ Paster is already listed but the
> hotkeys don't work, switch its entry **off and on again**. macOS binds these
> grants to the app's signing identity, and a stale entry can look enabled while
> granting nothing.

### How the two versions differ

| | Windows | macOS |
| --- | --- | --- |
| **Trigger key** | `Ctrl` | **`⌘` (Command)** |
| **Installer** | `.exe` (NSIS) | `.dmg` |
| **Lives in** | System tray | Menu bar |
| **First-run security prompt** | SmartScreen → *More info → Run anyway* | Gatekeeper → **right-click → Open**, or `xattr -dr com.apple.quarantine` |
| **Extra permissions** | None | **Accessibility** and **Input Monitoring** must be granted in System Settings → Privacy & Security, or the hotkeys cannot work |
| **Under the hood** | Win32 clipboard + low-level keyboard hook | `NSPasteboard` + `CGEventTap` |

Everything else — folders, the 9 slots, both modes, plain-text paste, slot
persistence, start-on-login — is shared code and behaves identically. The slot
store and the entire frontend are platform-independent.

**The one thing macOS users must do that Windows users don't:** grant
Accessibility **and** Input Monitoring. Both are required — with only the first,
the keyboard tap is created successfully and then never receives a single event,
so nothing appears to happen at all.

**Known macOS limitation:** the Noob-mode popup does not draw over another app's
full-screen Space.

Working on the port? See **[MACOS_PORT.md](MACOS_PORT.md)** — architecture map,
the platform differences, and the bugs that cost the most time, written up so
they don't have to be rediscovered.

---

## Project layout

```
index.html            # single frontend entry
src/
  main.ts             # branches on window label: "main" panel vs "popup" overlay
  styles.css
src-tauri/
  src/
    lib.rs            # Tauri app: state, commands, tray, windows
    hook.rs           # global keyboard grab + chord state machine + paste inject
    clipboard.rs      # raw snapshot/restore of all clipboard formats
    slots.rs          # SlotStore (9 slots) + FolderStore (folders)
  tauri.conf.json     # two windows: hidden "main" + frameless "popup"
MACOS_PORT.md         # handoff brief for the macOS port
```

Platform-specific code is isolated behind `#[cfg(...)]`; `slots.rs` and the whole
frontend are shared.

---

## Development

Requires Rust, Node, and — on Windows — the MSVC toolchain and the WebView2
runtime. On macOS, the Xcode Command Line Tools.

```bash
npm install
npm run tauri dev
```

Run the tests:

```bash
cd src-tauri && cargo test
```

## Build a release installer

```bash
npm run tauri build
```

Output lands in `src-tauri/target/release/bundle/` — `.exe` and `.msi` on
Windows, `.app` and `.dmg` on macOS.

---

## How the chord trick works

A low-level keyboard hook watches for `Ctrl`/`⌘` + digit and swallows the digit,
remembering the slot. Because the digit arrives **before** the letter, the hook
already knows it's a slot operation when `C`/`V` is pressed:

- On **`C`** it lets the app's normal copy happen, waits for the clipboard to
  actually change (watching the clipboard sequence number, not sleeping a fixed
  guess), snapshots it into the slot, then restores your previous clipboard.
- On **`V`** it **suppresses** the keystroke, loads the slot onto the clipboard,
  and injects a clean paste — so there's no double-paste, and normal `Ctrl+V`
  stays instant.

The hook callback itself does nothing but classify the keystroke and hand it to a
worker thread. That is deliberate: the OS silently disables a hook that takes too
long, which quietly breaks key suppression.
