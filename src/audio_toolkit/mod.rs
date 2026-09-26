//! The Handy fork's capture stack (cpal recorder, resampler, Silero VAD),
//! copied as is. Parts voice mode does not call (push-to-talk policies, level
//! callbacks) stay so the files remain diffable against the fork.
#![allow(dead_code)]

pub mod audio;
pub mod constants;
pub mod utils;
pub mod vad;

pub use audio::{list_input_devices, AudioRecorder, VadPolicy};
pub use utils::get_cpal_host;
pub use vad::{SileroVad, VoiceActivityDetector};
