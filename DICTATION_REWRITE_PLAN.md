# Plan: the dictation rewrite step (macOS)

Written 2026-09-22, to be built later. Everything here was measured on this
machine (M4 Pro, 24 GB, macOS 26.6.2) unless it says otherwise. Delete this
file when the work lands.

## Why

Dictation today writes down what you said. It does not fix a **self-correction**,
and that is the one flaw that changes meaning rather than polish:

| | |
|---|---|
| CQ today | "Let's meet tomorrow, actually no, let's meet Thursday at 8am." |
| Wanted | "Let's meet Thursday at 8am." |

The text contains two claims and only one is true. Everything else a rewrite
model was going to do — spelling, `snake_case`, times, quotation marks — the
whisper vocabulary prompt already does for free (§7.8). What is left is
**self-correction and filler**, and that is a much narrower job than the
original spike assumed.

## What was measured

A narrow prompt (`scratchpad/llama/narrow.txt`, reproduced in §Prompt below)
against 8 cases, including two traps where the model must **not** edit:

| Model | Score | Licence | Verdict |
|---|---|---|---|
| **Qwen2.5-7B-Instruct Q4_K_M** | **8/8** | Apache 2.0 | use this |
| Qwen2.5-3B-Instruct Q4_K_M | 5/8 | Qwen Research (non-commercial) | rejected twice over |

The 3B emitted invalid bytes and deleted real words ("I need the Zendesk
export" became "ed export"). Its licence rules it out regardless.

**Latency, on the exact sentence above, producing the correct output:**

| | |
|---|---|
| Model load | 427 ms |
| Prompt eval (159 tokens) | 427 ms |
| **Generation (10 tokens)** | **208 ms** |
| **Total** | **636 ms** |

An earlier estimate of "2–4 seconds" was wrong. It came from rewriting a
99-second dictation (555 tokens in, 222 out); generation is proportional to
output length, and a normal dictation is one or two sentences.

**Model:** `Qwen2.5-7B-Instruct-Q4_K_M.gguf`, 4,683,074,240 bytes, SHA-256
`65b8fcd92af6b4fefa935c625d1ac27ea29dcb6ee14589c55a8f115ceaaa1423`, from
`bartowski/Qwen2.5-7B-Instruct-GGUF`. Verified against the file on disk.
~6.2 GB resident while loaded.

## The costs, stated plainly

- **4.7 GB more to download and keep**, on top of the 547 MB speech model. The
  transcription half is the small half.
- **~6.2 GB of RAM** while the rewrite engine is loaded.
- **~200 ms** added to a dictation, once warm. This is no longer the objection.

## Architecture

`llama-server` as a **second sidecar**, exactly the shape `whisper-server`
already has: built by `scripts/build-whisper-server.sh` (extended), bundled via
`tauri.macos.conf.json`, spawned on demand, spoken to over loopback HTTP on an
OS-assigned port.

**Hiding the fixed costs.** Model load (427 ms) and prompt eval (427 ms) do not
depend on what is said, so both can happen **while the key is held**:

- On key-down, start the engine if it is not running and send a warm-up request
  carrying the system prompt.
- `llama-server` has `--cache-prompt` on by default: it compares each prompt to
  the previous one and evaluates only the unseen suffix, so the 159-token system
  prompt is paid for once and is free thereafter.
- Only generation (~200 ms) is left, and it cannot be overlapped — a correction
  is only knowable once the sentence has finished.

**Gesture.** Right `⌥` alone rewrites; right `⌥` + `Shift` pastes the raw
transcript. Shift already means "plain" in CQ's chord paste, so the meaning
carries over.

## The risk that matters

**A rewrite can silently delete a sentence.** Measured during the spike: the 7B
dropped "And I'll send the CSV beforehand" on 5 of 5 seeds, deterministically,
until the prompt was tightened. The output is fluent and simply missing
something, which is the worst kind of failure.

Three defences, in order:

1. **The Dictations jotpad already stores the raw transcript** (2beb149). It
   must keep storing the *raw* text, never the rewritten text.
2. **A content guard.** Before pasting, compare the rewrite to the transcript:
   every content word in the output must appear in the input (a rewrite only
   deletes, never invents), and the output must retain at least some fraction of
   the input's content words. On failure, paste the raw transcript and log it.
   This is pure logic and belongs in a tested module.
3. **The prompt itself**, which is the only reason 8/8 happened.

The guard's threshold needs deciding with real dictations, not invented ones —
a legitimate rewrite of "um, so, like, I think maybe Thursday" is a large
deletion.

## Prompt

Kept verbatim; it is the reason the score is 8/8. Any change to it re-runs the
corpus.

```
You clean up dictated speech. You make two kinds of edit and nothing else.

1. Self-correction. When the speaker corrects themselves, keep only the
   corrected version and delete the mistake and the words that flag it ("no",
   "actually", "I mean", "sorry", "wait"). Everything else in the sentence stays.
2. Filler. Delete "um", "uh", "like" and "you know" where they carry no meaning.

Change nothing else. Do not reword, reorder, summarise, add or reformat. If
there is nothing to correct, repeat the input exactly.

Return only the cleaned text.
```

Sampling: `--temp 0`. ChatML, as Qwen expects.

## Build order

Each step is testable on its own.

1. **`llama-server` as a sidecar.** Extend `scripts/build-whisper-server.sh` to
   build both servers; add `binaries/llama-server` to `tauri.macos.conf.json`.
   CI's placeholder step needs the second name too. **Check first** whether
   `llama-server` builds static with Metal embedded, as whisper's did.
2. **The second model in the download.** `MODELS` in `dictate.rs` is already a
   list and the panel already loops over it; this should be a table entry plus
   whatever the UI needs to show two rows and a 5.2 GB total.
3. **`dictate/rewrite.rs`.** The prompt, the client (curl, as elsewhere), the
   server lifecycle, and the guard. Pure parts tested: prompt assembly, the
   guard, ChatML framing.
4. **Pre-warm on key-down**, and measure that the warm path really is ~200 ms.
5. **`⌥`+`Shift` for the raw transcript**, and the setting for which is default.
6. **UI**: a switch in the dictation window, the second model's download row,
   and some indication when the guard rejected a rewrite.
7. **Docs**: README and `MACOS_PORT.md` §7.8, and delete this file.

## Verification

- **The 8-case corpus becomes a checked-in fixture**, run against any prompt
  change. It must include the two must-not-edit cases; those are what stop the
  model "fixing" text that was already right.
- **Guard tests**: a rewrite that drops a sentence is rejected; a legitimate
  filler-heavy rewrite is not.
- **Latency budget asserted in a real build**, not just measured once.
- **Windows**: untouched by construction, but `tauri.macos.conf.json` is the
  thing that makes it so, and CI is the check (see b0afdb7 for why this is not
  a formality).
- **By hand**: the user's own backtracking sentence, a dictation with nothing to
  correct, and a long dictation where the model might drop a clause.

## Open questions

- **Is 4.7 GB worth ~200 ms and one class of error?** Ask again after a week of
  using dictation as it is; the answer may be no.
- **Default on or off?** On is what makes it feel like Wispr Flow; off means
  nobody pays 6 GB of RAM unless they ask.
- **Should the engine idle out?** Releasing 6.2 GB after some minutes costs a
  427 ms reload on the next dictation. Probably worth it; needs a number.
- **Does the download belong in the same first-run panel**, or behind its own
  opt-in? 5.2 GB is a lot to present as one step.
