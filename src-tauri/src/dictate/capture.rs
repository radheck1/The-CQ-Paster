//! The microphone.
//!
//! Records into memory while the trigger is held, then writes a WAV for the
//! engine. Nothing is resampled here: whisper.cpp decodes with miniaudio,
//! configured as `ma_decoder_config_init(ma_format_f32, channels,
//! WHISPER_SAMPLE_RATE)`, so it converts rate, format and channel count
//! itself. Verified by sending it 48 kHz stereo — the shape a Mac microphone
//! actually hands you — and getting back the same transcript as the 16 kHz
//! mono original. A hand-rolled decimator here would only be worse.
//!
//! cpal's stream is not `Send` on macOS, so it cannot be parked in a static
//! and stopped from elsewhere. Instead a thread owns the stream for as long as
//! the recording lasts and drops it when asked, which is also the only way to
//! be sure the device is released.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};

/// Longer than this and we stop on our own. A trigger key that goes missing —
/// released while another app had grabbed the tap, say — must not leave the
/// microphone open indefinitely.
const MAX_RECORDING: Duration = Duration::from_secs(300);

/// One input device, as the chooser shows it.
#[derive(Clone, Serialize)]
pub struct MicInfo {
    /// cpal's own device identifier, written out as text. This is what gets
    /// remembered: it survives two microphones sharing a name, which a name
    /// alone would not.
    pub id: String,
    /// What to call it in the chooser.
    pub name: String,
    /// Who makes it, when the driver says. Shown to separate two devices with
    /// the same name.
    pub maker: Option<String>,
    /// The one macOS would pick right now.
    pub is_default: bool,
}

/// Which microphone dictation should use.
///
/// `serde(default)` on the whole struct, not just the `Option`s: a missing
/// `bool` is a hard parse error otherwise, so a settings file written by an
/// older build — or half-written — would fail to load and quietly discard the
/// user's choice instead of keeping the part that is still there.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MicChoice {
    /// `None` means "whatever macOS is using", which is the default and
    /// follows AirPods in and out without being asked.
    pub device: Option<String>,
    /// The chosen device's name when it was chosen. Kept so an unplugged
    /// microphone can be named in the message rather than shown as an id.
    pub name: Option<String>,
    /// Always use `device`. When it is missing CQ still records — off the
    /// system default — and says that is what it did.
    pub locked: bool,
}

/// What `start` ended up opening, so the window can report a substitution
/// rather than let a recording come back in the wrong voice.
#[derive(Clone, Serialize)]
pub struct Started {
    pub device: String,
    /// Set when a locked microphone was not there and the default was used.
    pub instead_of: Option<String>,
}

fn choice_file() -> PathBuf {
    crate::data_dir().join("dictation-mic.json")
}

pub fn choice() -> MicChoice {
    std::fs::read(choice_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn set_choice(c: &MicChoice) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(c).map_err(|e| e.to_string())?;
    let path = choice_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Written beside itself and renamed, so a crash mid-write cannot leave a
    // half-file that parses as "no choice".
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("cannot save the choice: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot save the choice: {e}"))?;
    Ok(())
}

/// Every input device macOS offers — built-in, AirPods, USB interfaces and
/// virtual devices alike.
pub fn devices() -> Vec<MicInfo> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| id_of(&d));
    let Ok(found) = host.input_devices() else {
        return Vec::new();
    };
    let mut out: Vec<MicInfo> = Vec::new();
    for d in found {
        let Some(id) = id_of(&d) else { continue };
        // A device that cannot report a config is not one we could record
        // from, so listing it would only offer a choice that fails later.
        if d.default_input_config().is_err() {
            continue;
        }
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        let desc = d.description().ok();
        let name = desc
            .as_ref()
            .map(|x| x.name().to_string())
            .unwrap_or_else(|| "Unknown microphone".into());
        let maker = desc.as_ref().and_then(|x| x.manufacturer().map(str::to_string));
        out.push(MicInfo {
            is_default: Some(&id) == default.as_ref(),
            id,
            name,
            maker,
        });
    }
    out
}

/// cpal's device id as text, which is what a choice is stored as.
fn id_of(d: &cpal::Device) -> Option<String> {
    d.id().ok().map(|i| i.to_string())
}

fn name_of(d: &cpal::Device) -> String {
    d.description()
        .ok()
        .map(|x| x.name().to_string())
        .unwrap_or_else(|| "the default microphone".into())
}

/// The device to open, and what it stands in for.
fn pick(host: &cpal::Host) -> Result<(cpal::Device, Started), String> {
    let want = choice();

    if let Some(id) = want.device.as_deref() {
        let found = host
            .input_devices()
            .ok()
            .and_then(|mut ds| ds.find(|d| id_of(d).as_deref() == Some(id)));
        if let Some(d) = found {
            let name = name_of(&d);
            return Ok((d, Started { device: name, instead_of: None }));
        }
        if want.locked {
            // Locked but absent. Recording off the default beats not recording,
            // as long as the substitution is visible rather than silent — a
            // swap you cannot see only shows up as a transcript of the wrong
            // room.
            let d = host
                .default_input_device()
                .ok_or("there is no microphone available")?;
            let got = name_of(&d);
            let missing = want.name.clone().unwrap_or_else(|| id.to_string());
            crate::diag(&format!(
                "dictate: locked microphone \"{missing}\" is not connected — using \"{got}\""
            ));
            return Ok((d, Started { device: got, instead_of: Some(missing) }));
        }
    }

    let d = host
        .default_input_device()
        .ok_or("there is no microphone available")?;
    let got = name_of(&d);
    Ok((d, Started { device: got, instead_of: None }))
}

struct Active {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<i16>>>,
    rate: u32,
    channels: u16,
    done: std::thread::JoinHandle<()>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

pub fn is_recording() -> bool {
    ACTIVE.lock().unwrap().is_some()
}

/// Open the microphone and start filling a buffer. Returns once the device is
/// running, so the caller can show that it is listening without lying.
pub fn start() -> Result<Started, String> {
    let mut guard = ACTIVE.lock().unwrap();
    if guard.is_some() {
        return Err("already listening".into());
    }

    let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    // The thread reports what the device actually gave us, which is rarely
    // what was asked for.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, u16, Started), String>>();

    let done = {
        let samples = samples.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let built = build_stream(&samples);
            match built {
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
                Ok((stream, rate, channels, started)) => {
                    if let Err(e) = stream.play() {
                        let _ = tx.send(Err(format!("cannot start the microphone: {e}")));
                        return;
                    }
                    let _ = tx.send(Ok((rate, channels, started)));
                    let began = std::time::Instant::now();
                    while !stop.load(Ordering::SeqCst) && began.elapsed() < MAX_RECORDING {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    // Dropping the stream here, on the thread that made it, is
                    // what actually closes the device.
                    drop(stream);
                }
            }
        })
    };

    let (rate, channels, started) = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "the microphone did not respond".to_string())??;

    *guard = Some(Active {
        stop,
        samples,
        rate,
        channels,
        done,
    });
    Ok(started)
}

fn build_stream(
    samples: &Arc<Mutex<Vec<i16>>>,
) -> Result<(cpal::Stream, u32, u16, Started), String> {
    let host = cpal::default_host();
    let (device, started) = pick(&host)?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("cannot read the microphone's settings: {e}"))?;
    // In cpal 0.18 these are plain `u32` / `u16`, not newtypes.
    let rate = config.sample_rate();
    let channels = config.channels();
    let cfg = config.config();
    let err = |e| crate::diag(&format!("dictate: microphone error: {e}"));

    // Whatever the device's native format is, it is stored as i16 and the
    // engine converts from there.
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let buf = samples.clone();
            device.build_input_stream(
                cfg.clone(),
                move |data: &[f32], _: &_| {
                    let chunk: Vec<i16> = data
                        .iter()
                        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                        .collect();
                    super::indicator::set_level(super::indicator::loudness(&chunk));
                    buf.lock().unwrap().extend_from_slice(&chunk);
                },
                err,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let buf = samples.clone();
            device.build_input_stream(
                cfg.clone(),
                move |data: &[i16], _: &_| {
                    super::indicator::set_level(super::indicator::loudness(data));
                    buf.lock().unwrap().extend_from_slice(data);
                },
                err,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let buf = samples.clone();
            device.build_input_stream(
                cfg.clone(),
                move |data: &[u16], _: &_| {
                    let chunk: Vec<i16> =
                        data.iter().map(|s| (*s as i32 - 32768) as i16).collect();
                    super::indicator::set_level(super::indicator::loudness(&chunk));
                    buf.lock().unwrap().extend_from_slice(&chunk);
                },
                err,
                None,
            )
        }
        other => return Err(format!("this microphone's format is not supported ({other:?})")),
    }
    .map_err(|e| format!("cannot open the microphone: {e}"))?;

    Ok((stream, rate, channels, started))
}

/// Stop recording and write what was captured. `None` when there is nothing
/// worth transcribing.
pub fn stop(into: &Path) -> Result<Option<PathBuf>, String> {
    let Some(active) = ACTIVE.lock().unwrap().take() else {
        return Ok(None);
    };
    active.stop.store(true, Ordering::SeqCst);
    let _ = active.done.join();

    let samples = active.samples.lock().unwrap();
    let frames = samples.len() / active.channels.max(1) as usize;
    let seconds = frames as f32 / active.rate.max(1) as f32;
    if seconds < 0.25 {
        // Too short to be speech. Transcribing it wastes seconds and invites
        // whisper to invent a word from noise.
        crate::diag(&format!("dictate: {seconds:.2}s captured — too short, discarded"));
        return Ok(None);
    }

    let wav = write_wav(into, &samples, active.rate, active.channels)?;
    crate::diag(&format!(
        "dictate: {seconds:.1}s captured at {} Hz, {} ch",
        active.rate, active.channels
    ));
    Ok(Some(wav))
}

/// A 16-bit PCM WAV. Written by hand rather than with a crate: the header is
/// 44 bytes of known layout, and this way it can be tested without a device.
pub fn wav_header(samples: usize, rate: u32, channels: u16) -> Vec<u8> {
    let bits: u16 = 16;
    let block_align = channels * bits / 8;
    let byte_rate = rate * block_align as u32;
    let data_len = (samples * 2) as u32;
    let mut h = Vec::with_capacity(44);
    h.extend(b"RIFF");
    // Everything after this field: the header's remaining 36 bytes plus data.
    h.extend((36 + data_len).to_le_bytes());
    h.extend(b"WAVEfmt ");
    h.extend(16u32.to_le_bytes()); // PCM fmt chunk size
    h.extend(1u16.to_le_bytes()); // PCM, uncompressed
    h.extend(channels.to_le_bytes());
    h.extend(rate.to_le_bytes());
    h.extend(byte_rate.to_le_bytes());
    h.extend(block_align.to_le_bytes());
    h.extend(bits.to_le_bytes());
    h.extend(b"data");
    h.extend(data_len.to_le_bytes());
    h
}

fn write_wav(path: &Path, samples: &[i16], rate: u32, channels: u16) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut f = std::fs::File::create(path).map_err(|e| format!("cannot write the recording: {e}"))?;
    f.write_all(&wav_header(samples.len(), rate, channels))
        .map_err(|e| format!("cannot write the recording: {e}"))?;
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        bytes.extend(s.to_le_bytes());
    }
    f.write_all(&bytes)
        .map_err(|e| format!("cannot write the recording: {e}"))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_survives_being_written_and_read() {
        // The file is the only memory of which microphone was picked, so a
        // round trip through it has to keep every field.
        let c = MicChoice {
            device: Some("coreaudio:BuiltInMic".into()),
            name: Some("MacBook Pro Microphone".into()),
            locked: true,
        };
        let back: MicChoice = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back.device.as_deref(), Some("coreaudio:BuiltInMic"));
        assert_eq!(back.name.as_deref(), Some("MacBook Pro Microphone"));
        assert!(back.locked);
    }

    #[test]
    fn no_choice_means_follow_the_system() {
        // A missing or unreadable file must read as "follow the default",
        // never as a lock on a device nobody picked.
        let d = MicChoice::default();
        assert!(d.device.is_none());
        assert!(!d.locked);
        let from_junk: MicChoice = serde_json::from_str("{}").unwrap();
        assert!(from_junk.device.is_none());
        assert!(!from_junk.locked);
    }

    #[test]
    fn an_older_choice_without_a_name_still_loads() {
        // `name` was added after `device` and `locked`; a file written before
        // it must not fail to parse and silently reset the user's choice.
        let c: MicChoice =
            serde_json::from_str(r#"{"device":"x","locked":true}"#).unwrap();
        assert_eq!(c.device.as_deref(), Some("x"));
        assert!(c.locked);
        assert!(c.name.is_none());
    }

    #[test]
    fn the_header_is_a_wav_a_decoder_will_accept() {
        // 16 000 mono samples: one second at 16 kHz.
        let h = wav_header(16_000, 16_000, 1);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(&h[36..40], b"data");
        // RIFF size counts everything after itself, not the whole file.
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 32_000);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 32_000);
    }

    #[test]
    fn the_header_describes_the_format_it_was_given() {
        // 48 kHz stereo, which is what a Mac microphone usually offers.
        let h = wav_header(96_000, 48_000, 2);
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 48_000);
        // Byte rate and block align have to agree with the rest or players
        // read the samples at the wrong speed.
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 4);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 48_000 * 4);
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
    }

    #[test]
    fn an_empty_recording_still_makes_a_valid_header() {
        let h = wav_header(0, 16_000, 1);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36);
    }

    #[test]
    fn a_written_file_is_header_plus_samples() {
        let dir = std::env::temp_dir().join(format!("cq-wav-{}", std::process::id()));
        let p = dir.join("r.wav");
        let samples: Vec<i16> = (0..1000).map(|i| (i * 7) as i16).collect();
        write_wav(&p, &samples, 16_000, 1).unwrap();
        let on_disk = std::fs::read(&p).unwrap();
        assert_eq!(on_disk.len(), 44 + samples.len() * 2);
        assert_eq!(&on_disk[0..4], b"RIFF");
        // Little-endian, so the first sample's bytes come back in that order.
        assert_eq!(i16::from_le_bytes(on_disk[44..46].try_into().unwrap()), 0);
        assert_eq!(i16::from_le_bytes(on_disk[46..48].try_into().unwrap()), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
