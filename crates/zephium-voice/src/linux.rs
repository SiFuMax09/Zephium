//! Linux: capture from the default microphone through `parec` (PulseAudio and
//! PipeWire's pulse server) or, failing that, `arecord` (ALSA). Both write raw
//! 16 kHz mono float to a pipe, so no audio library is linked and no audio
//! reaches the disk. There is no on-device recognizer here; takes are
//! uploaded.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::{Core, Fault, Listener, Outcome, Recognition, SPEECH_RATE};

/// 100 ms of mono f32 per read, which matches the cadence of the other
/// platforms' device callbacks.
const BLOCK_BYTES: usize = (SPEECH_RATE as usize / 10) * 4;

pub(crate) struct Capture {
    core: Arc<Core>,
    stop: Arc<AtomicBool>,
    child: Arc<std::sync::Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

fn recorders() -> [(&'static str, Vec<String>); 2] {
    let rate = SPEECH_RATE.to_string();
    [
        (
            "parec",
            vec![
                "--raw".into(),
                "--format=float32le".into(),
                format!("--rate={rate}"),
                "--channels=1".into(),
                "--latency-msec=40".into(),
            ],
        ),
        (
            "arecord",
            vec![
                "-q".into(),
                "-t".into(),
                "raw".into(),
                "-f".into(),
                "FLOAT_LE".into(),
                "-r".into(),
                rate,
                "-c".into(),
                "1".into(),
            ],
        ),
    ]
}

fn spawn_recorder(candidates: &[(&str, Vec<String>)]) -> Result<Child, Fault> {
    for (program, arguments) in candidates {
        let spawned = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(child) = spawned {
            return Ok(child);
        }
    }
    Err(Fault::NoMicrophone)
}

/// Decodes the whole little-endian f32 samples in `bytes` into `samples` and
/// returns how many bytes that used; a trailing partial sample stays for the
/// next read.
fn decode(bytes: &[u8], samples: &mut Vec<f32>) -> usize {
    samples.clear();
    let whole = bytes.len() - bytes.len() % 4;
    samples.extend(bytes[..whole].chunks_exact(4).map(|sample| {
        let value = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
        if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }));
    whole
}

impl Capture {
    pub(crate) fn start(
        recognition: Recognition,
        pause: bool,
        listener: Arc<dyn Listener>,
    ) -> Result<Self, Fault> {
        Self::start_with(&recorders(), recognition, pause, listener)
    }

    fn start_with(
        candidates: &[(&str, Vec<String>)],
        recognition: Recognition,
        pause: bool,
        listener: Arc<dyn Listener>,
    ) -> Result<Self, Fault> {
        if recognition != Recognition::Upload {
            return Err(Fault::LanguageUnavailable);
        }
        let mut child = spawn_recorder(candidates)?;
        let Some(mut output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Fault::NoMicrophone);
        };
        let core = Arc::new(Core::new(f64::from(SPEECH_RATE), true, pause, listener));
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(std::sync::Mutex::new(Some(child)));
        let worker_core = core.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("zephium-voice".into())
            .spawn(move || {
                let mut buffer = vec![0_u8; BLOCK_BYTES];
                let mut carried = 0_usize;
                let mut samples = Vec::with_capacity(BLOCK_BYTES / 4);
                loop {
                    match output.read(&mut buffer[carried..]) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let filled = carried + read;
                            let whole = decode(&buffer[..filled], &mut samples);
                            worker_core.push(&samples);
                            buffer.copy_within(whole..filled, 0);
                            carried = filled - whole;
                        }
                    }
                }
                if !worker_stop.load(Ordering::Acquire) {
                    // The recorder exited by itself: no server, or the device
                    // went away.
                    worker_core.listener().ended(crate::Ending::Lost);
                }
            })
            .map_err(|_| Fault::NoMicrophone)?;
        let capture = Self {
            core,
            stop,
            child,
            worker: Some(worker),
        };
        // A missing server makes the recorder exit at once; report that as no
        // microphone instead of an empty take.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let exited = capture
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.as_mut().and_then(|child| child.try_wait().ok()))
            .flatten()
            .is_some();
        if exited {
            drop(capture);
            return Err(Fault::NoMicrophone);
        }
        Ok(capture)
    }

    fn release(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.lock().ok().and_then(|mut child| child.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    pub(crate) fn finish(mut self) -> Result<Outcome, Fault> {
        self.release();
        let (take, heard) = self.core.stop();
        Ok(Core::upload(take, heard))
    }

    pub(crate) fn cancel(mut self) {
        self.release();
        self.core.stop();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.release();
    }
}

/// The locale variables in the order the C library consults them, as BCP 47
/// tags: `de_DE.UTF-8` becomes `de-DE`. `LANGUAGE` may list several.
pub(crate) fn preferred_languages() -> Vec<String> {
    let mut tags = Vec::new();
    let variables = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];
    for name in variables {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        for part in value.split(':') {
            if let Some(tag) = locale_tag(part) {
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        }
        if !tags.is_empty() && name != "LANGUAGE" {
            break;
        }
    }
    tags.truncate(8);
    tags
}

fn locale_tag(locale: &str) -> Option<String> {
    let base = locale.split(['.', '@']).next()?;
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    let tag = base.replace('_', "-");
    let valid = tag.split('-').all(|part| {
        !part.is_empty() && part.len() <= 8 && part.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    valid.then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        levels: std::sync::Mutex<usize>,
        endings: std::sync::Mutex<Vec<crate::Ending>>,
    }

    impl Listener for Recorder {
        fn levels(&self, levels: &[f32]) {
            *self.levels.lock().unwrap() += levels.len();
        }
        fn ended(&self, ending: crate::Ending) {
            self.endings.lock().unwrap().push(ending);
        }
    }

    /// A recorder that replays a synthetic second of tone, then idles like a
    /// live device until it is killed.
    fn tone_recorder(directory: &std::path::Path) -> (String, Vec<String>) {
        let path = directory.join("tone.raw");
        let bytes: Vec<u8> = (0..SPEECH_RATE)
            .flat_map(|n| (0.3 * (n as f32 * 0.07).sin()).to_le_bytes())
            .collect();
        std::fs::write(&path, bytes).unwrap();
        (
            "sh".into(),
            vec![
                "-c".into(),
                format!("cat '{}'; exec sleep 30", path.display()),
            ],
        )
    }

    #[test]
    fn a_take_from_the_recorder_is_metered_and_returned_as_audio() {
        let directory = tempfile::tempdir().unwrap();
        let (program, arguments) = tone_recorder(directory.path());
        let listener = Arc::new(Recorder::default());
        let capture = Capture::start_with(
            &[(program.as_str(), arguments)],
            Recognition::Upload,
            false,
            listener.clone(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        match capture.finish().unwrap() {
            Outcome::Audio { wav, duration_ms } => {
                assert_eq!(duration_ms, 1_000);
                assert_eq!(wav.len(), 44 + 32_000);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(*listener.levels.lock().unwrap(), 25);
        assert!(listener.endings.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelling_stops_the_recorder_without_reporting_a_loss() {
        let directory = tempfile::tempdir().unwrap();
        let (program, arguments) = tone_recorder(directory.path());
        let listener = Arc::new(Recorder::default());
        let capture = Capture::start_with(
            &[(program.as_str(), arguments)],
            Recognition::Upload,
            false,
            listener.clone(),
        )
        .unwrap();
        capture.cancel();
        assert!(listener.endings.lock().unwrap().is_empty());
    }

    #[test]
    fn a_missing_or_failing_recorder_is_no_microphone() {
        let missing = [("zephium-no-such-recorder", Vec::new())];
        let failing = [("false", Vec::new())];
        for candidates in [&missing[..], &failing[..]] {
            let outcome = Capture::start_with(
                candidates,
                Recognition::Upload,
                false,
                Arc::new(Recorder::default()),
            );
            assert!(matches!(outcome, Err(Fault::NoMicrophone)));
        }
    }

    #[test]
    fn on_device_recognition_is_not_offered() {
        let outcome = Capture::start_with(
            &[],
            Recognition::OnDevice {
                locale: "de-DE".into(),
            },
            false,
            Arc::new(Recorder::default()),
        );
        assert!(matches!(outcome, Err(Fault::LanguageUnavailable)));
    }

    #[test]
    fn locales_become_bcp47_tags() {
        assert_eq!(locale_tag("de_DE.UTF-8").as_deref(), Some("de-DE"));
        assert_eq!(locale_tag("sr_RS@latin").as_deref(), Some("sr-RS"));
        assert_eq!(locale_tag("en").as_deref(), Some("en"));
        assert_eq!(locale_tag("C"), None);
        assert_eq!(locale_tag("POSIX"), None);
        assert_eq!(locale_tag(""), None);
        assert_eq!(locale_tag("../etc"), None);
    }

    #[test]
    fn a_partial_trailing_sample_is_carried_not_decoded() {
        let mut samples = Vec::new();
        let mut bytes = 0.5_f32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2]);
        assert_eq!(decode(&bytes, &mut samples), 4);
        assert_eq!(samples, vec![0.5]);
    }

    #[test]
    fn non_finite_and_loud_samples_are_bounded() {
        let mut samples = Vec::new();
        let bytes: Vec<u8> = [f32::NAN, f32::INFINITY, 3.0, -3.0]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        decode(&bytes, &mut samples);
        assert_eq!(samples, vec![0.0, 0.0, 1.0, -1.0]);
    }
}
