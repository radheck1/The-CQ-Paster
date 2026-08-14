# CQ Paster

An ultra-minimal, hotkey-driven **multi-slot clipboard manager** for Windows
(macOS planned). Copy up to **9 things** into numbered slots and paste them back
in any order — without ever leaving the keyboard.

## Hotkeys

| Action | Keys |
| --- | --- |
| Copy selection into slot *N* | `Ctrl` + `<N>` + `C` |
| Paste slot *N* | `Ctrl` + `<N>` + `V` |
| Normal copy / paste | `Ctrl+C` / `Ctrl+V` (unchanged) |

`N` is `1`–`9`. **Press the digit before the letter** — e.g. hold `Ctrl`, tap
`2`, tap `C` to store the current selection in slot 2; later hold `Ctrl`, tap
`2`, tap `V` to paste it.

It works with **anything you can copy** — text, images, files, and app-specific
formats — because each slot stores a byte-exact snapshot of every clipboard
format and writes it back on paste. Your real clipboard is preserved: after a
slot paste, whatever you had on the clipboard is restored.

### The one tradeoff

While CQ Paster is running, `Ctrl+1`…`Ctrl+9` become its trigger prefix, so
those specific combos no longer reach the foreground app (e.g. browser
tab-switching). Plain number typing is unaffected.

## Modes

- **Master** — zero UI. Just you, the hotkeys, and your memory of what's where.
- **Noob** — a small reference popup appears next to your cursor showing all 9
  slots whenever you start a chord. You still paste with the hotkey.

Toggle modes from the tray icon or the control panel. The app lives in the
**system tray**; closing the control-panel window keeps it running.

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
    slots.rs          # the 9-slot store
  tauri.conf.json     # two windows: hidden "main" + frameless "popup"
```

## Development

Requires Rust (MSVC toolchain), Node, and the WebView2 runtime.

```bash
npm install
npm run tauri dev
```

## Build a release installer

```bash
npm run tauri build
```

Produces a `.msi` / `.exe` under `src-tauri/target/release/bundle/`.

## How the chord trick works

A low-level keyboard hook (`rdev` grab) watches for `Ctrl` + digit and swallows
the digit, remembering the slot. Because the digit arrives **before** the
letter, the hook already knows it's a slot operation when `C`/`V` is pressed:

- On `C` it lets the normal copy happen, then snapshots the clipboard into the
  slot.
- On `V` it **suppresses** the keystroke, loads the slot onto the clipboard,
  and injects a clean paste — so there's no double-paste and normal `Ctrl+V`
  stays instant.
