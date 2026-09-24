# cQ

A Tauri v2 multi-slot clipboard manager for Windows and macOS. One codebase,
mostly shared. macOS additionally hosts cQ Jotter (notepad with reminders),
cQ Shotter (screenshots) and dictation.

`MACOS_PORT.md` is the record of how the macOS side works and why it works that
way. Read the section covering whatever you are about to change — most of it was
written because something went wrong first.

## Rules that don't bend

**Never break the Windows build.** macOS-only Rust sits behind
`#[cfg(target_os = "macos")]`, frontend code behind `IS_MAC`, capability files
carry `"platforms": ["macOS"]`. A Mac never compiles the Windows side, so a green
local build proves nothing about it. **CI on a pull request is the only real
check** — when it hasn't run yet, say so plainly instead of claiming the change
is safe. Reading the diff and concluding it looks fine has broken the build
twice.

**Nothing slow in the keyboard hook callback** (`src-tauri/src/hook/macos.rs`).
The budget there is an atomic read and a channel send; the worker thread does
everything else. A single `println!` in it got the event tap disabled outright,
and the same mistake on Windows let suppressed keys leak through.

**Measure before theorising.** The app writes to `diagnostics.log`. Read it, or
add a `diag` line and have the user reproduce, before naming a cause. Guesses
that sounded right have cost this project real work — including a confident
latency claim that turned out to come from one 99-second outlier.

**Ask before anything that touches the machine or the remote**: quitting and
reinstalling cQ, committing, pushing, opening a pull request, merging. Approval
for one of these is not approval for the next one.

**Don't use the user's Terminal panel.** Run commands through your own tools.

**`jotter.json` belongs to the running app.** cQ owns it while it is running and
will overwrite what you write. Read it freely; leave editing to the user.

**Ask before designing.** For any UI or behaviour change, offer options with a
recommendation and wait. Don't infer what was wanted from a one-line request.

## Before you write code

Load the `cq-engineering` skill in `.claude/skills/`. It has the build and
install sequence, how to verify a change is actually Windows-safe, the
measurement recipes, the files that must not be touched, and the traps that have
already caught someone — including the `generate_handler!` gate trap, which
compiles perfectly on a Mac and fails on Windows.
