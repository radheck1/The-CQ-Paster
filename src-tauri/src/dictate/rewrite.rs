//! Cleaning up what was dictated.
//!
//! Whisper writes down what was said. This turns it into what the speaker
//! meant to type, and it makes exactly two kinds of edit:
//!
//!   * a **self-correction** — "let's meet tomorrow, actually no, Thursday"
//!     becomes "let's meet Thursday", which is the one flaw that changes
//!     meaning rather than tidiness: the raw text asserts two things and only
//!     one of them is true;
//!   * **filler** — "um", "uh", "like", "you know".
//!
//! Nothing else. It may not reword, reorder, summarise or reformat, and that
//! restraint is what makes it safe to check: a rewrite can only ever *delete*,
//! so any word in the output that was not in the input means it went wrong.
//! Everything whisper already does well — spelling, `snake_case`, times,
//! quotation marks — is handled by the vocabulary prompt (`vocab.rs`) and is
//! deliberately not asked for again here.
//!
//! ## The failure this guards against
//!
//! A rewrite drops a sentence silently. Measured during the spike: the model
//! dropped "And I'll send the CSV beforehand" on five seeds out of five, and
//! the result reads perfectly — it is simply missing something. So the output
//! is checked before it is used, and the raw transcript is kept regardless
//! (it goes to the Dictations jotpad either way).
//!
//! ## Cost
//!
//! `llama-server` holds the model and caches the prompt prefix, so the
//! 160-token instruction is evaluated once and is 17 tokens thereafter.
//! Measured: 649 ms for the first rewrite after launch, then 244–355 ms.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an unused engine keeps its 6 GB.
///
/// Long enough to cover a stretch of dictating — a second dictation inside
/// this window costs nothing — and short enough that a machine left alone gets
/// its memory back. The cost of being wrong is small either way: the reload
/// starts when the trigger goes down, so it happens while the user is still
/// speaking.
const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
/// How often the idle check runs. Coarse on purpose; nothing here is urgent.
const IDLE_TICK: Duration = Duration::from_secs(30);
/// How long a dictation will wait for an engine that is still starting.
///
/// The trigger already started it, so this is the tail of a reload that began
/// while the user was speaking. Past this it is better to paste the transcript
/// than to leave someone staring at nothing: 4.4 GB off a cold disk took 17.5 s
/// when it was measured, which is not a wait anyone would accept mid-sentence.
const READY_GRACE: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Context only has to hold the instruction plus one dictation.
const CONTEXT: u32 = 4096;

/// The instruction. Changing a word of this re-runs the corpus in `tests`:
/// the 8/8 score belongs to this text, not to the model.
pub const INSTRUCTION: &str = "\
You clean up dictated speech. You make two kinds of edit and nothing else.

1. Self-correction. When the speaker corrects themselves, keep only the corrected version and delete the mistake and the words that flag it (\"no\", \"actually\", \"I mean\", \"sorry\", \"wait\"). Everything else in the sentence stays.
2. Filler. Delete \"um\", \"uh\", \"like\" and \"you know\" where they carry no meaning.

Change nothing else. Do not reword, reorder, summarise, add or reformat. If there is nothing to correct, repeat the input exactly.

Return only the cleaned text.";

struct Server {
    child: Child,
    port: u16,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// When the engine was last asked for anything. `None` while it is not running.
static LAST_USED: Mutex<Option<Instant>> = Mutex::new(None);

fn touch() {
    *LAST_USED.lock().unwrap() = Some(Instant::now());
}

/// Release the engine once it has been idle long enough. Started with the app;
/// it costs one comparison every half minute and gives back about 6 GB.
pub fn watch_idle() {
    std::thread::spawn(|| loop {
        std::thread::sleep(IDLE_TICK);
        let idle_for = LAST_USED.lock().unwrap().map(|t| t.elapsed());
        if let Some(d) = idle_for {
            if d >= IDLE_AFTER && SERVER.lock().unwrap().is_some() {
                crate::diag(&format!(
                    "dictate: rewrite engine idle for {} min — releasing its memory",
                    d.as_secs() / 60
                ));
                stop();
            }
        }
    });
}

/// Qwen's chat format. Built here rather than using the server's chat endpoint
/// so the exact prompt that was measured is the exact prompt that is sent.
pub fn chatml(instruction: &str, text: &str) -> String {
    format!(
        "<|im_start|>system\n{instruction}<|im_end|>\n<|im_start|>user\n{text}<|im_end|>\n<|im_start|>assistant\n"
    )
}

/// Words that carry content, lowercased, punctuation removed. Used only by the
/// guard, which compares what came out against what went in.
pub fn content_words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Why a rewrite was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A word appeared that was not in the transcript. The model was asked
    /// only to delete, so this means it rewrote or invented something.
    Invented(String),
    /// Too much of the transcript is gone. A dropped sentence looks exactly
    /// like a very thorough filler removal, so this is a threshold rather than
    /// a certainty — which is why the raw transcript is kept either way.
    TooShort { kept: usize, had: usize },
    /// Nothing came back.
    Empty,
}

/// Words the model may add without it counting as invention.
///
/// Removing a correction often needs a joining word back — "send it to Marcus,
/// I mean Priya" loses its comma and gains nothing, but other repairs do. These
/// carry no meaning on their own, so their appearance cannot change what the
/// sentence says. A single fixed threshold rejected real rewrites over "the".
const INSERTABLE: &[&str] = &[
    "a", "an", "and", "at", "be", "by", "do", "for", "in", "is", "it", "of", "on",
    "or", "the", "to", "was", "were", "will", "with",
];

/// Words that mark a speaker correcting themselves.
///
/// Their presence changes what a large reduction means. Two false starts and
/// most of what was said is meant to be thrown away — measured: 18 words in, 5
/// out, and the 5 were right. Without a marker, nothing should disappear
/// except filler, so a large cut is the sentence-dropping failure instead.
const CORRECTION_MARKERS: &[&str] =
    &["no", "wait", "actually", "sorry", "mean", "nevermind", "scratch", "rather"];

/// How much may go when the speaker corrected themselves.
const MIN_KEPT_CORRECTED: f64 = 0.2;
/// How much may go when they did not. Only filler should be leaving.
const MIN_KEPT_PLAIN: f64 = 0.7;

/// Did the speaker correct themselves?
pub fn has_correction(transcript: &str) -> bool {
    content_words(transcript)
        .iter()
        .any(|w| CORRECTION_MARKERS.contains(&w.as_str()))
}

/// Check a rewrite against the transcript it came from.
pub fn check(transcript: &str, rewritten: &str) -> Result<(), Refused> {
    let out = content_words(rewritten);
    if out.is_empty() {
        return Err(Refused::Empty);
    }
    let mut had: Vec<String> = content_words(transcript);
    if had.is_empty() {
        return Ok(()); // nothing to compare against
    }
    let total = had.len();
    // Multiset containment: every output word must be spent against an input
    // word, so a word repeated more often than it was said is caught too.
    for w in &out {
        match had.iter().position(|h| h == w) {
            Some(i) => {
                had.remove(i);
            }
            // A word from nowhere is only alarming if it carries meaning. A
            // joining word cannot change what the sentence says; a name, a
            // number or a verb can.
            None if INSERTABLE.contains(&w.as_str()) => {}
            None => return Err(Refused::Invented(w.clone())),
        }
    }
    let kept = total - had.len();
    let floor = if has_correction(transcript) {
        MIN_KEPT_CORRECTED
    } else {
        MIN_KEPT_PLAIN
    };
    if (kept as f64) < floor * total as f64 {
        return Err(Refused::TooShort { kept, had: total });
    }
    Ok(())
}

fn model() -> Option<std::path::PathBuf> {
    super::model_path("rewrite")
}

/// Is the rewrite model downloaded?
pub fn available() -> bool {
    model().map(|p| p.exists()).unwrap_or(false)
}

fn sidecar() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bundled = dir.join("llama-server");
    if bundled.exists() {
        return Some(bundled);
    }
    let dev = dir.parent().and_then(|p| p.parent()).map(|p| p.join("binaries"))?;
    for name in [
        "llama-server-aarch64-apple-darwin",
        "llama-server-x86_64-apple-darwin",
    ] {
        let c = dev.join(name);
        if c.exists() {
            return Some(c);
        }
    }
    None
}

pub fn server_args(model: &Path, port: u16) -> Vec<String> {
    vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        CONTEXT.to_string(),
        // Everything on the GPU; the whole point is that this is fast.
        "-ngl".into(),
        "99".into(),
    ]
}

/// Ready means the model is loaded, which `/health` reports and an open socket
/// does not — the server accepts connections while still loading, and a
/// request sent then comes back empty.
fn healthy(port: u16) -> bool {
    Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--max-time",
            "2",
            &format!("http://127.0.0.1:{port}/health"),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("\"status\":\"ok\""))
        .unwrap_or(false)
}

/// Start the engine if it is not running, and return its port.
pub fn ensure_running() -> Result<u16, String> {
    ensure_running_within(START_TIMEOUT)
}

/// As `ensure_running`, but give up after `budget`. A dictation uses a short
/// one because the trigger has already been holding the key; a pre-warm uses
/// the full timeout because nobody is waiting on it.
pub fn ensure_running_within(budget: Duration) -> Result<u16, String> {
    let mut guard = SERVER.lock().unwrap();
    if let Some(s) = guard.as_mut() {
        match s.child.try_wait() {
            Ok(None) if healthy(s.port) => {
                touch();
                return Ok(s.port);
            }
            _ => {
                let _ = s.child.kill();
                *guard = None;
            }
        }
    }
    let model = model().ok_or("the rewrite model is not downloaded yet")?;
    if !model.exists() {
        return Err("the rewrite model is not downloaded yet".into());
    }
    let bin = sidecar().ok_or("the rewrite engine is missing from this build")?;
    let port = super::engine::free_port().map_err(|e| format!("no free port: {e}"))?;
    let child = Command::new(&bin)
        .args(server_args(&model, port))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the rewrite engine: {e}"))?;
    crate::diag(&format!("dictate: rewrite engine starting on port {port}"));

    let mut server = Server { child, port };
    let began = Instant::now();
    while began.elapsed() < budget {
        if let Ok(Some(status)) = server.child.try_wait() {
            return Err(format!("the rewrite engine stopped at once ({status})"));
        }
        if healthy(port) {
            crate::diag(&format!(
                "dictate: rewrite engine ready in {:.1}s",
                began.elapsed().as_secs_f32()
            ));
            *guard = Some(server);
            touch();
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = server.child.kill();
    Err("the rewrite engine did not start in time".into())
}

/// Start the engine and evaluate the instruction, so the first real dictation
/// does not pay for either. Called when the trigger goes down.
pub fn prewarm() {
    std::thread::spawn(|| {
        if !available() {
            return;
        }
        // The full timeout here: nothing is waiting on a pre-warm.
        if let Ok(port) = ensure_running_within(START_TIMEOUT) {
            // A token of output is enough to get the instruction into the
            // prompt cache; the answer is thrown away.
            let _ = ask(port, "warm up.", 1);
        }
    });
}

fn ask(port: u16, text: &str, n_predict: u32) -> Result<String, String> {
    let body = serde_json::json!({
        "prompt": chatml(INSTRUCTION, text),
        "n_predict": n_predict,
        "temperature": 0,
        // The instruction is identical every time, so the server re-evaluates
        // only the dictation itself: 160 tokens become 17.
        "cache_prompt": true,
        "stop": ["<|im_end|>"],
    });
    let out = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            &REQUEST_TIMEOUT.as_secs().to_string(),
            "-X",
            "POST",
            &format!("http://127.0.0.1:{port}/completion"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
        ])
        .output()
        .map_err(|e| format!("cannot reach the rewrite engine: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the rewrite engine refused: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("the rewrite engine sent something unreadable: {e}"))?;
    Ok(v.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string())
}

/// Clean up a transcript. Returns the transcript unchanged if the rewrite
/// cannot be trusted — the point is to improve the text, never to risk it.
pub fn clean(transcript: &str) -> String {
    if transcript.trim().is_empty() || !available() {
        return transcript.to_string();
    }
    let began = Instant::now();
    // Short budget: the trigger started the engine while this was being
    // spoken, so anything still missing is a cold reload, and waiting out a
    // cold reload mid-sentence is worse than pasting what was said.
    let port = match ensure_running_within(READY_GRACE) {
        Ok(p) => p,
        Err(e) => {
            crate::diag(&format!("dictate: no rewrite ({e}) — pasting as heard"));
            return transcript.to_string();
        }
    };
    // Room for the whole transcript back, plus a little.
    let budget = (content_words(transcript).len() * 3 + 64) as u32;
    let rewritten = match ask(port, transcript, budget) {
        Ok(t) => t,
        Err(e) => {
            crate::diag(&format!("dictate: no rewrite ({e})"));
            return transcript.to_string();
        }
    };
    match check(transcript, &rewritten) {
        Ok(()) => {
            crate::diag(&format!(
                "dictate: rewritten in {:.0} ms",
                began.elapsed().as_secs_f32() * 1000.0
            ));
            rewritten
        }
        Err(why) => {
            // The raw transcript is what gets pasted. It is also already in the
            // Dictations jotpad, so nothing is lost either way.
            crate::diag(&format!("dictate: rewrite rejected ({why:?}), pasting as heard"));
            transcript.to_string()
        }
    }
}

pub fn stop() {
    if let Some(mut s) = SERVER.lock().unwrap().take() {
        let _ = s.child.kill();
        let _ = s.child.wait();
        crate::diag("dictate: rewrite engine stopped");
    }
    *LAST_USED.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_is_framed_the_way_qwen_expects() {
        let p = chatml("RULES", "hello");
        assert!(p.starts_with("<|im_start|>system\nRULES<|im_end|>"));
        assert!(p.ends_with("<|im_start|>assistant\n"));
        assert!(p.contains("<|im_start|>user\nhello<|im_end|>"));
    }

    #[test]
    fn the_instruction_still_forbids_rewording() {
        // The guard only works because the model may delete and nothing else.
        // If this instruction ever permits rewording, `check` must change too.
        assert!(INSTRUCTION.contains("Do not reword"));
        assert!(INSTRUCTION.contains("nothing else"));
    }

    #[test]
    fn content_words_ignore_punctuation_and_case() {
        assert_eq!(content_words("Let's meet Thursday!"), ["lets", "meet", "thursday"]);
        assert_eq!(content_words("  "), Vec::<String>::new());
        // Underscores survive: `created_at` is one word, not two.
        assert_eq!(content_words("check created_at now"), ["check", "created_at", "now"]);
    }

    #[test]
    fn a_real_correction_passes() {
        assert_eq!(
            check(
                "Let's meet tomorrow, actually no, let's meet Thursday at 8am.",
                "Let's meet Thursday at 8am."
            ),
            Ok(())
        );
    }

    #[test]
    fn an_unchanged_sentence_passes() {
        let s = "Can you check whether the API is returning null for updated_at?";
        assert_eq!(check(s, s), Ok(()));
    }

    #[test]
    fn an_invented_word_is_refused() {
        // The model was told to delete only. A word from nowhere means it
        // rewrote, and a rewrite that reworded could also have dropped a clause.
        assert_eq!(
            check("We should meet Thursday.", "We should convene Thursday."),
            Err(Refused::Invented("convene".into()))
        );
        // Any invented word is enough, wherever it falls.
        assert!(matches!(
            check("Let's meet Thursday.", "Let us convene on Thursday."),
            Err(Refused::Invented(_))
        ));
    }

    #[test]
    fn a_dropped_sentence_is_refused() {
        // The measured failure: fluent output, quietly missing a clause. There
        // is no correction here, so nothing should be going except filler.
        let had = "Let's meet Wednesday at 3:30. And I'll send the CSV beforehand. \
                   Ticket 48213 is the one that started all this.";
        let lost = "Let's meet Wednesday at 3:30.";
        assert!(!has_correction(had));
        assert!(matches!(check(had, lost), Err(Refused::TooShort { .. })));
    }

    #[test]
    fn two_false_starts_may_take_most_of_the_sentence_with_them() {
        // Measured on a real dictation: 18 words in, 5 out, and the 5 were
        // right. The first guard rejected this, which threw away a good
        // rewrite and pasted the stumbling version instead.
        let had = "Let's do the review on Monday. No, wait, Monday's the holiday. \
                   Let's do Tuesday at 2:00. Actually, 2:30.";
        assert!(has_correction(had));
        assert_eq!(check(had, "Let's review Tuesday at 2:30."), Ok(()));
    }

    #[test]
    fn a_joining_word_is_not_invention() {
        // Measured: real rewrites were rejected over "the".
        assert_eq!(check("send report to Priya", "send the report to Priya"), Ok(()));
        // ...but a word that carries meaning still is.
        assert!(matches!(
            check("send report to Priya", "send report to Marcus"),
            Err(Refused::Invented(_))
        ));
    }

    #[test]
    fn a_correction_marker_does_not_excuse_losing_everything() {
        // Even with a correction, an answer this short is not a rewrite.
        let had = "No, actually, the deploy went out at nine and the dashboard \
                   is showing errors already, can you roll it back";
        assert!(matches!(check(had, "roll back"), Err(Refused::TooShort { .. })));
    }

    #[test]
    fn heavy_filler_removal_is_allowed() {
        // Legitimate, and a large cut: the guard must not mistake it for loss.
        assert_eq!(
            check("um so like I think maybe Thursday works", "so I think maybe Thursday works"),
            Ok(())
        );
    }

    #[test]
    fn an_empty_rewrite_is_refused() {
        assert_eq!(check("something was said", ""), Err(Refused::Empty));
        assert_eq!(check("something was said", "   "), Err(Refused::Empty));
    }

    #[test]
    fn a_word_repeated_more_than_it_was_said_is_refused() {
        // Multiset, not set: a stutter loop is a real failure mode.
        assert!(matches!(
            check("meet Thursday", "meet Thursday Thursday"),
            Err(Refused::Invented(_))
        ));
    }

    #[test]
    fn the_engine_is_told_to_use_the_gpu_and_listen_only_here() {
        let a = server_args(Path::new("/m.gguf"), 4321);
        let h = a.iter().position(|x| x == "--host").unwrap();
        assert_eq!(a[h + 1], "127.0.0.1");
        let n = a.iter().position(|x| x == "-ngl").unwrap();
        assert_eq!(a[n + 1], "99");
    }
}
