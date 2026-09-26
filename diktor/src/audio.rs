//! Microphone for voice mode only: Handy's `AudioRecordingManager` without
//! push-to-talk, settings, mute or level events. The recorder, VAD and
//! resampler underneath (`audio_toolkit`) are the fork's, unchanged.
use crate::audio_toolkit::{
    list_input_devices,
    vad::{
        SmoothedVad, VAD_OFFLINE_HANGOVER_FRAMES, VAD_ONSET_FRAMES, VAD_PREFILL_FRAMES,
        VAD_STREAMING_HANGOVER_FRAMES,
    },
    AudioRecorder, SileroVad, VadPolicy,
};
use crate::engine::VoiceRoute;
use crate::voice_session::state::lock;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Normal (non-voice) Silero threshold the recorder restores between
/// sessions; voice mode applies its own stricter `VAD_VOICE_*` profile.
pub(crate) const VAD_THRESHOLD: f32 = 0.3;

/// First-sample notification of one capture start.
pub struct RecordingReadiness(mpsc::Receiver<()>);

impl RecordingReadiness {
    pub fn wait_timeout(self, timeout: Duration) -> bool {
        self.0.recv_timeout(timeout).is_ok()
    }
}

/// `diktor daemon --input-wav`: replayed at every voice start instead of
/// opening the microphone (end-to-end checks without any audio device).
pub struct WavInput {
    samples: Arc<Vec<f32>>,
    sample_rate: u32,
}

impl WavInput {
    /// Any PCM WAV; channels are averaged to mono like a microphone.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut reader =
            hound::WavReader::open(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let spec = reader.spec();
        let channels = usize::from(spec.channels.max(1));
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = (1_i64 << (spec.bits_per_sample.max(1) - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|s| s as f32 / scale))
                    .collect::<Result<_, _>>()?
            }
        };
        let samples = interleaved
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect();
        Ok(Self {
            samples: Arc::new(samples),
            sample_rate: spec.sample_rate,
        })
    }
}

/// Silence before the WAV starts, so the session is listening when it speaks.
const WAV_LEAD_IN: Duration = Duration::from_millis(800);

pub struct VoiceAudio {
    /// `None` opens the system default input.
    device: Option<String>,
    wav: Option<WavInput>,
    vad_model: PathBuf,
    route: Arc<VoiceRoute>,
    recorder: Mutex<Option<AudioRecorder>>,
    recording: AtomicBool,
}

impl VoiceAudio {
    pub fn new(
        device: Option<String>,
        wav: Option<WavInput>,
        vad_model: PathBuf,
        route: Arc<VoiceRoute>,
    ) -> Self {
        Self {
            device,
            wav,
            vad_model,
            route,
            recorder: Mutex::new(None),
            recording: AtomicBool::new(false),
        }
    }

    fn create_recorder(&self) -> anyhow::Result<AudioRecorder> {
        let silero = SileroVad::new(&self.vad_model, VAD_THRESHOLD).map_err(|e| {
            anyhow::anyhow!("Silero VAD {} not loaded: {e}", self.vad_model.display())
        })?;
        let smoothed = SmoothedVad::new(
            Box::new(silero),
            VAD_PREFILL_FRAMES,
            VAD_OFFLINE_HANGOVER_FRAMES,
            VAD_ONSET_FRAMES,
        );
        let frames = Arc::clone(&self.route);
        let activity = Arc::clone(&self.route);
        Ok(AudioRecorder::new()
            .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {e}"))?
            .with_vad(
                Box::new(smoothed),
                VAD_THRESHOLD,
                VAD_ONSET_FRAMES,
                VAD_OFFLINE_HANGOVER_FRAMES,
                VAD_STREAMING_HANGOVER_FRAMES,
            )
            .with_audio_callback(move |frame| frames.feed(frame))
            .with_voice_activity_callback(move |speech| activity.voice_activity(speech)))
    }

    /// Refuses a configured microphone that is not there instead of letting
    /// the sound server fall back to another one.
    pub fn check_input(&self) -> anyhow::Result<()> {
        if self.wav.is_some() {
            return Ok(());
        }
        self.resolve_device()?;
        check_pipewire_node()
    }

    fn resolve_device(&self) -> anyhow::Result<Option<cpal::Device>> {
        let Some(name) = &self.device else {
            return Ok(None);
        };
        let devices =
            list_input_devices().map_err(|e| anyhow::anyhow!("Input devices not listed: {e}"))?;
        devices
            .into_iter()
            .find(|d| &d.name == name)
            .map(|d| Some(d.device))
            .ok_or_else(|| {
                anyhow::anyhow!("Input device {name:?} not found (see `diktor devices`)")
            })
    }

    /// Opens the microphone and starts a voice capture: every frame goes to
    /// the model, the VAD only times phrase boundaries.
    pub fn start_voice_recording(&self) -> Result<RecordingReadiness, String> {
        let mut recorder = lock(&self.recorder);
        if recorder.is_none() {
            *recorder = Some(self.create_recorder().map_err(|e| e.to_string())?);
        }
        let rec = recorder.as_mut().ok_or("Recorder not available")?;
        match &self.wav {
            Some(wav) => rec.open_wav(Arc::clone(&wav.samples), wav.sample_rate, WAV_LEAD_IN),
            None => {
                let device = self.resolve_device().map_err(|e| e.to_string())?;
                rec.open(device)
            }
        }
        .map_err(|e| format!("Failed to open recorder: {e}"))?;
        let ready = rec
            .start_with_retention(VadPolicy::Disabled, false)
            .map_err(|e| format!("Failed to start recorder: {e}"))?;
        self.recording.store(true, Ordering::Release);
        Ok(RecordingReadiness(ready))
    }

    pub fn voice_boundary(&self, boundary: Box<dyn FnOnce() + Send>) -> Result<(), String> {
        lock(&self.recorder)
            .as_ref()
            .ok_or("Recorder is not available")?
            .voice_boundary(boundary)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    pub fn voice_capture_healthy(&self) -> bool {
        self.is_recording()
            && lock(&self.recorder)
                .as_ref()
                .is_some_and(|recorder| !recorder.needs_reopen())
    }

    /// Stops capture and closes the microphone: nothing listens while off.
    pub fn cancel_recording(&self) {
        if !self.recording.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(recorder) = lock(&self.recorder).as_mut() {
            let _ = recorder.stop();
            let _ = recorder.close();
        }
    }
}

/// `diktor devices`: names accepted by `input_device` in the config.
pub fn device_names() -> anyhow::Result<Vec<(String, bool)>> {
    Ok(list_input_devices()
        .map_err(|e| anyhow::anyhow!("Input devices not listed: {e}"))?
        .into_iter()
        .map(|d| (d.name, d.is_default))
        .collect())
}

/// `PIPEWIRE_NODE` pins the ALSA stream to one PipeWire node; if that node is
/// missing PipeWire silently records from the default source instead. Refuse.
fn check_pipewire_node() -> anyhow::Result<()> {
    let Ok(node) = std::env::var("PIPEWIRE_NODE") else {
        return Ok(());
    };
    let output = std::process::Command::new("pw-dump")
        .output()
        .map_err(|e| anyhow::anyhow!("PIPEWIRE_NODE={node} set, but pw-dump failed: {e}"))?;
    let objects: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let serial = node.parse::<u64>().ok();
    let found = objects.as_array().is_some_and(|objects| {
        objects.iter().any(|object| {
            let props = &object["info"]["props"];
            props["node.name"] == node.as_str()
                || serial.is_some_and(|serial| props["object.serial"].as_u64() == Some(serial))
        })
    });
    anyhow::ensure!(
        found,
        "PIPEWIRE_NODE={node}: no such PipeWire node; refusing to fall back to another microphone"
    );
    Ok(())
}
