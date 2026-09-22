//! The speech engine: a `whisper-server` process bundled beside CQ.
//!
//! Whisper is not compiled into CQ. It ships as its own executable and is
//! spoken to over HTTP on the loopback interface. That keeps the C++ out of
//! the Cargo build — so Windows CI never sees it — lets an inference crash
//! take down only the engine, and means the model can be unloaded by ending a
//! process rather than hoping an allocator gives 6 GB back.
//!
//! The server holds the model in memory between requests, which is the whole
//! point of a server rather than a one-shot command: loading the 547 MB model
//! costs seconds, transcribing a sentence costs a fraction of one.
//!
//! ## Two flags that are not adjustable
//!
//! `-nt` (no timestamps) is **never** passed. Measured on a 99-second
//! recording, it made whisper drop an entire sentence and garble another, the
//! same result on every run — 155 words against 164 with timestamps left on.
//! Timestamp tokens are part of how the decoder tracks where it is, and the
//! transcript is worse without them. The `text` the server returns has no
//! timestamps in it anyway, so there is nothing gained by asking.
//!
//! `-bs 5` (beam search) is always passed. Greedy decoding dropped a sentence
//! from the same recording when run through the command-line tool.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Given up on after this, so a wedged engine reports rather than hangs.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(120);

struct Server {
    child: Child,
    port: u16,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// Where the bundled engine lives: beside CQ's own executable, which is where
/// Tauri puts an `externalBin`. In a `cargo run` build there is no bundle, so
/// a copy under `src-tauri/binaries` is used instead and dictation works in
/// development too.
fn sidecar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bundled = dir.join("whisper-server");
    if bundled.exists() {
        return Some(bundled);
    }
    // Development: target/debug/cq-paster -> src-tauri/binaries/
    let dev = dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("binaries"))?;
    let triple = dev.join(format!("whisper-server-{}", std::env::consts::ARCH))
        .with_extension("");
    for candidate in [
        dev.join("whisper-server-aarch64-apple-darwin"),
        dev.join("whisper-server-x86_64-apple-darwin"),
        triple,
    ] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// A port the OS says is free. Asked for rather than fixed, so two copies of
/// CQ — or anything else already on 8080 — cannot collide.
fn free_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

/// Is the engine answering on this port yet?
fn responding(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(250),
    )
    .is_ok()
}

/// The engine's command line. Pure, so the flags above can be asserted in a
/// test rather than trusted.
pub fn server_args(model: &Path, port: u16) -> Vec<String> {
    vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        // Beam search: greedy decoding drops sentences (see the module note).
        "-bs".into(),
        "5".into(),
        "-l".into(),
        "en".into(),
        // Deliberately absent: `-nt`. It loses words.
    ]
}

/// Start the engine if it is not already running, and return its port.
pub fn ensure_running() -> Result<u16, String> {
    let mut guard = SERVER.lock().unwrap();
    if let Some(s) = guard.as_mut() {
        match s.child.try_wait() {
            Ok(None) if responding(s.port) => return Ok(s.port),
            // Died, or is no longer listening: fall through and start again.
            _ => {
                let _ = s.child.kill();
                *guard = None;
            }
        }
    }

    let model = super::model_path("whisper").ok_or("the speech model is not downloaded yet")?;
    if !model.exists() {
        return Err("the speech model is not downloaded yet".into());
    }
    let bin = sidecar().ok_or("the speech engine is missing from this build")?;
    let port = free_port().map_err(|e| format!("no free port: {e}"))?;

    let child = Command::new(&bin)
        .args(server_args(&model, port))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the speech engine: {e}"))?;
    crate::diag(&format!("dictate: engine starting on port {port}"));

    let mut server = Server { child, port };
    let began = Instant::now();
    while began.elapsed() < START_TIMEOUT {
        if let Ok(Some(status)) = server.child.try_wait() {
            return Err(format!("the speech engine stopped at once ({status})"));
        }
        if responding(port) {
            crate::diag(&format!(
                "dictate: engine ready in {:.1}s",
                began.elapsed().as_secs_f32()
            ));
            *guard = Some(server);
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = server.child.kill();
    Err("the speech engine did not start in time".into())
}

/// Transcribe a WAV file. Blocking; callers run it off the main thread.
pub fn transcribe(wav: &Path) -> Result<String, String> {
    let port = ensure_running()?;
    // curl again, for the same reasons as the model download: it is already on
    // the machine, and multipart POST is one flag rather than a dependency.
    let mut args: Vec<String> = vec![
        "--silent".into(),
        "--show-error".into(),
        "--fail-with-body".into(),
        "--max-time".into(),
        TRANSCRIBE_TIMEOUT.as_secs().to_string(),
        "--form".into(),
        format!("file=@{}", wav.to_string_lossy()),
        "--form".into(),
        "response_format=json".into(),
    ];
    // The vocabulary, if there is one. Sent per request rather than fixed at
    // startup so editing the list takes effect on the next dictation instead
    // of the next launch.
    let prompt = super::vocab::prompt_from(&super::vocab::load().terms);
    if !prompt.is_empty() {
        args.push("--form".into());
        args.push(format!("prompt={prompt}"));
        // Without this the prompt primes only the first 30-second window, so
        // a longer dictation loses the vocabulary halfway through.
        args.push("--form".into());
        args.push("carry_initial_prompt=true".into());
    }
    args.push(format!("http://127.0.0.1:{port}/inference"));

    let out = Command::new("/usr/bin/curl")
        .args(&args)
        .output()
        .map_err(|e| format!("cannot reach the speech engine: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the speech engine refused the recording: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let body: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("the speech engine sent something unreadable: {e}"))?;
    let text = body
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or("the speech engine sent no text")?;
    Ok(tidy(text))
}

/// The server returns one line per segment, each with a leading space. Joining
/// them is all the tidying the text needs — there are no timestamps in it.
pub fn tidy(raw: &str) -> String {
    raw.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// End the engine, giving back the memory the model occupies.
pub fn stop() {
    if let Some(mut s) = SERVER.lock().unwrap().take() {
        let _ = s.child.kill();
        let _ = s.child.wait();
        crate::diag("dictate: engine stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag that cost a sentence. Asserted rather than remembered.
    #[test]
    fn the_engine_is_never_asked_to_drop_timestamps() {
        let a = server_args(Path::new("/m.bin"), 1234);
        assert!(
            !a.iter().any(|x| x == "-nt" || x == "--no-timestamps"),
            "-nt makes whisper drop whole sentences: 155 words against 164 on \
             the same recording, the same result every run"
        );
    }

    #[test]
    fn beam_search_is_always_on() {
        let a = server_args(Path::new("/m.bin"), 1234);
        let i = a.iter().position(|x| x == "-bs").expect("no beam size");
        assert_eq!(a[i + 1], "5", "greedy decoding also drops sentences");
    }

    #[test]
    fn the_engine_listens_only_to_this_machine() {
        let a = server_args(Path::new("/m.bin"), 4321);
        let i = a.iter().position(|x| x == "--host").unwrap();
        assert_eq!(a[i + 1], "127.0.0.1", "the engine must not be reachable off-box");
        let p = a.iter().position(|x| x == "--port").unwrap();
        assert_eq!(a[p + 1], "4321");
    }

    #[test]
    fn a_free_port_is_actually_free() {
        let p = free_port().unwrap();
        assert!(p > 0);
        // Asking twice should not hand back a port still held by the first ask.
        assert!(std::net::TcpListener::bind(("127.0.0.1", p)).is_ok());
    }

    #[test]
    fn tidy_joins_the_segments_the_server_returns() {
        let raw = " Okay, so I need to pull the data.\n Here are three things.\n";
        assert_eq!(tidy(raw), "Okay, so I need to pull the data. Here are three things.");
        assert_eq!(tidy(""), "");
        assert_eq!(tidy("\n\n  \n"), "");
        // A single segment keeps its words and loses only the padding.
        assert_eq!(tidy("  hello  "), "hello");
    }
}
