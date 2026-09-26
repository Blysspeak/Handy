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

pub struct VoiceAudio {
    /// `None` opens the system default input.
    device: Option<String>,
    vad_model: PathBuf,
    route: Arc<VoiceRoute>,
    recorder: Mutex<Option<AudioRecorder>>,
    recording: AtomicBool,
}

impl VoiceAudio {
    pub fn new(device: Option<String>, vad_model: PathBuf, route: Arc<VoiceRoute>) -> Self {
        Self {
            device,
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
        let device = self.resolve_device().map_err(|e| e.to_string())?;
        let rec = recorder.as_mut().ok_or("Recorder not available")?;
        rec.open(device)
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
