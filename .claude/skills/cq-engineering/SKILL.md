---
name: cq-engineering
description: How to build, install, verify and ship changes to cQ (~/The-CQ-Paster — the Tauri v2 clipboard manager that also hosts cQ Jotter, cQ Shotter and dictation). Use this whenever work is happening in that repo: writing or changing Rust or frontend code, adding a Tauri command, debugging behaviour, judging whether a change is safe for the Windows build, running the tests, building the .app, installing it for the user, or committing and opening a pull request. Also use it when asked why something in cQ works the way it does, or before telling the user a change is safe. Read it before writing code rather than after — several rules here exist because they were learned by breaking the build, and they are not discoverable from the diff.
---

# Building and verifying cQ

The rules that don't bend are in `CLAUDE.md` at the repo root and are always
loaded. This is the rest: how to actually do the work, and the traps that have
already caught someone.

## Where everything is

```
src-tauri/src/
  lib.rs          commands, tray menu, data_dir(), diag(), log rotation
  hook/macos.rs   the CGEventTap — the hot path, see CLAUDE.md
  clipboard/      capture and restore, per platform
  slots.rs        the 9 clipboard slots and folders
  jotter.rs       the notepad;  reminders.rs  shotter.rs  shake.rs
  permissions.rs  every user-facing permission prompt lives here
  dictate/        capture, engine (whisper), rewrite (llama), vocab, harvest,
                  focus, spacing, listing, pending, indicator, record
src/              main.ts, dictate.ts, jotter.ts, shotter.ts, styles.css
scripts/          build-whisper-server.sh, tint-logo.swift
```

State lives in `~/Library/Application Support/com.cqpaster.app/`:

| File | What it is |
|---|---|
| `diagnostics.log` | every `diag()` line; rolls at 2 MB, keeps one generation |
| `dictations.log` | one JSON line per dictation; capped at 512 KB |
| `folders.bin` | slots and folders |
| `jotter.json` | the notepad — the app's, not yours |
| `shotter.json`, `shake.json` | screenshots list, shake sensitivity |
| `dictation-mic.json`, `dictation-vocab.json`, `dictation-lists.json` | one small JSON file per setting; follow this pattern for new ones |
| `models/` | 4.7 GB Qwen2.5-7B + 574 MB whisper. **Never delete.** Re-downloading costs the user an hour |

The product is named **cQ** but the bundle identifier is still
`com.cqpaster.app`, deliberately: the data directory and the granted macOS
permissions both key off it. `signingIdentity` is `"CQ Paster Self Signed"`, a
certificate name in the user's keychain. Neither is a leftover to tidy up.

## Build, then ask before installing

```bash
npm run tauri build -- --target universal-apple-darwin
```

Takes several minutes; run it in the background. Verify the result is genuinely
universal with `lipo -archs` rather than trusting the target name.

Installing means quitting the app the user is using, so **ask first, every
time**. Then, in this order:

1. **Quit gracefully** — `osascript -e 'tell application "cQ" to quit'`, and
   wait for the process to go. Not `pkill`: the app owns `jotter.json` and
   writes state on quit.
2. Clear stray sidecars (`whisper-server`, `llama-server`) if they outlived it.
3. Copy the new `.app` into `/Applications`, replacing the old one.
4. Relaunch and **read the log to confirm it came up**:

```
launch: exe=Ok("/Applications/cQ.app/Contents/MacOS/cQ")
permissions at startup: accessibility=true input_monitoring_raw=0 (granted)
CGEventTapCreate succeeded.
```

Those three lines are the difference between "installed" and "working". If
accessibility or input monitoring came back false, the permission did not
survive and the hotkeys are dead — say so rather than reporting success.

Renaming the binary or moving the bundle can invalidate the macOS permission
grants, because they attach to the executable. Warn before an install that does
either, then check the log afterwards.

## Windows safety: how to actually check

`.github/workflows/ci.yml` compiles and tests **both** platforms on every push
and pull request. It deliberately does not run `tauri build` — that would need
the signing certificate, which is a local keychain identity.

CI is the only real check, because a Mac never compiles the Windows side. Two
specific traps have got past careful reading:

**`generate_handler!` gates one entry, not a block.** Each command carries its
own `#[cfg(target_os = "macos")]`, and the attribute binds to the single entry
after it. Adding a command by inserting a line *above* an existing one silently
steals that entry's gate:

```rust
#[cfg(target_os = "macos")]
dictate::dictate_lists,      // takes the gate
dictate::dictate_set_lists,  // now ungated — breaks Windows
dictate::dictate_mics,       // and so does this
```

This compiles and passes every test on a Mac. `cargo test windows_build` reads
`lib.rs` through `include_str!` and catches it; run it after touching the
handler list.

**`externalBin` is validated at compile time** by `tauri-build`, so a sidecar
declared in `tauri.conf.json` makes Windows look for a `.exe` that does not
exist. Platform-specific bundle config belongs in `tauri.macos.conf.json`.

## Measuring instead of guessing

```bash
tail -40 ~/Library/Application\ Support/com.cqpaster.app/diagnostics.log
```

For dictation, `dictations.log` holds what was heard, what the clean-up made of
it, and which was pasted — the ground truth for anything about transcription
quality.

When a rule or threshold needs tuning, **run it over the real corpus** rather
than over invented examples. `dictate/listing.rs` has the pattern: an
`#[ignore]`d test that reads the local `dictations.log` and prints what the code
would do.

```bash
cargo test --lib -- --ignored --nocapture report_against_every_recorded_dictation
```

That test is why the list splitter needs two breaks rather than one, and why one
of its markers was deleted. Neither was predictable from reasoning.

When behaviour depends on another application — what the accessibility tree
reports, which apps answer a query — a standalone probe run from a shell will
not work, because the permission belongs to cQ and not to the probe. Add a
`diag` line, ship it, and read the log a day later.

## Tests that earn their place

**Don't let tests write to the user's data directory.** `crate::diag` appends to
the real `diagnostics.log`, so a test calling a function that logs will pollute
it — interleaved and confusing, in the one file used for diagnosis. Keep the
pure function separate from its logging wrapper and test the pure one. Check by
comparing the file's byte count before and after a run.

**Prove a new test catches the thing it was written for.** Reintroduce the bug,
watch the test fail and name the right location, then restore. A test written
from the same misunderstanding as the bug passes for the wrong reason.

**Guard a source-reading test against passing vacuously.** If it scans for a
pattern and the pattern stops matching, it silently protects nothing — so assert
that it still finds what it is checking.

**Never log a character of the user's text.** Editable fields include password
fields. Log which branch was taken, not the content.

## What not to touch

- **`dictate/rewrite.rs`'s `INSTRUCTION`.** Its 8/8 corpus score belongs to that
  exact wording. Changing a word means re-running the corpus. A formatting-heavy
  version of it is what silently dropped sentences — the narrowness is the
  mitigation, and the guard is only the second line.
- **`jotter.json`** while the app is running.
- The bundle identifier and the signing identity, as above.

## Shipping

Work on a branch off the current one; never commit to `main`. `MACOS_PORT.md`
gets the story of the change — including **what was tried and abandoned, and the
measurement that killed it**, since the next person will otherwise try it again.
The dictation sections are the model for the level of detail.

Open the pull request, then wait for both CI jobs. Report a failure with its
actual output rather than summarising it away, and fix the cause rather than the
symptom.
