use anyhow::Result;

pub const VAD_PREFILL_FRAMES: usize = 15;
pub const VAD_OFFLINE_HANGOVER_FRAMES: usize = 15;
pub const VAD_STREAMING_HANGOVER_FRAMES: usize = 55;
pub const VAD_ONSET_FRAMES: usize = 2;

/// Persistent voice mode sends every captured frame to the model; its VAD only
/// times phrase boundaries. A stricter profile there cannot drop words, it only
/// keeps room noise, breaths and clicks from restarting the silence countdown.
pub const VAD_VOICE_THRESHOLD: f32 = 0.7;
/// 150 ms of consecutive speech frames before voice activity starts.
pub const VAD_VOICE_ONSET_FRAMES: usize = 5;
/// Voice mode never uses the 1.65 s streaming tail.
pub const VAD_VOICE_HANGOVER_FRAMES: usize = VAD_OFFLINE_HANGOVER_FRAMES;

pub enum VadFrame<'a> {
    /// Speech – may aggregate several frames (prefill + current + hangover)
    Speech(&'a [f32]),
    /// Non-speech (silence, noise). Down-stream code can ignore it.
    Noise,
}

impl<'a> VadFrame<'a> {
    #[inline]
    pub fn is_speech(&self) -> bool {
        matches!(self, VadFrame::Speech(_))
    }
}

pub trait VoiceActivityDetector: Send + Sync {
    /// Primary streaming API: feed one 30-ms frame, get keep/drop decision.
    fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> Result<VadFrame<'a>>;

    fn is_voice(&mut self, frame: &[f32]) -> Result<bool> {
        Ok(self.push_frame(frame)?.is_speech())
    }

    /// Set the post-speech hangover tail (in 30 ms frames) applied to
    /// subsequent frames. Detectors without a smoothing tail can ignore this.
    fn set_hangover_frames(&mut self, _frames: usize) {}

    /// Set the speech probability a frame must exceed. Detectors without a
    /// probability can ignore this.
    fn set_threshold(&mut self, _threshold: f32) {}

    /// Set how many consecutive speech frames start speech. Detectors without
    /// onset smoothing can ignore this.
    fn set_onset_frames(&mut self, _frames: usize) {}

    fn reset(&mut self) {}
}

mod silero;
mod smoothed;

pub use silero::SileroVad;
pub use smoothed::SmoothedVad;
