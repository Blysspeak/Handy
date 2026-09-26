pub mod state;

use crate::audio::VoiceAudio;
use crate::engine::{voice_error_code, Transcriber};
use state::{lock, BoundaryMarker, Snapshot, Status, Target, VoiceQueue, COMMIT_TIMEOUT};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Owns persistent voice lifecycle (the Handy fork's `VoiceSessionManager`
/// without the Tauri app and overlay). Model work never runs on the audio
/// thread; stopping invalidates permission to deliver synchronously.
///
/// No desktop bridge yet: every phrase is delivered to the log.
pub struct VoiceSessionManager {
    audio: Arc<VoiceAudio>,
    transcription: Arc<Transcriber>,
    session: Mutex<Option<Arc<VoiceQueue>>>,
    generation: AtomicU64,
    busy: AtomicBool,
}

/// Stands in for the focused window until the desktop bridge is wired in.
fn log_target() -> Target {
    Target {
        id: "log".into(),
        app_id: "log".into(),
        pid: std::process::id(),
        output: String::new(),
        output_x: 0,
        output_y: 0,
    }
}

impl VoiceSessionManager {
    pub fn new(audio: Arc<VoiceAudio>, transcription: Arc<Transcriber>) -> Arc<Self> {
        Arc::new(Self {
            audio,
            transcription,
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
        })
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        lock(&self.session).as_ref().map(|queue| queue.snapshot())
    }

    pub fn toggle(self: &Arc<Self>) {
        if self.busy.load(Ordering::Acquire) {
            self.stop();
            return;
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let queue = Arc::new(VoiceQueue::new(generation));
        *lock(&self.session) = Some(Arc::clone(&queue));
        let manager = Arc::clone(self);
        std::thread::spawn(move || manager.run(queue));
    }

    fn run(self: Arc<Self>, queue: Arc<VoiceQueue>) {
        let mut captured = false;
        let startup = (|| -> Result<(), String> {
            if self.audio.is_recording() || self.transcription.voice_worker_busy() {
                return Err("busy".into());
            }
            self.publish(&queue);
            let ready = self
                .transcription
                .start_voice_session(Arc::clone(&queue))
                .map_err(|error| {
                    log::error!("Voice start: {error:#}");
                    voice_error_code(&error).to_string()
                })?;
            if !queue.active() {
                return Ok(());
            }
            let capture_ready = self.audio.start_voice_recording().map_err(|error| {
                log::error!("Voice microphone: {error}");
                "microphone".to_string()
            })?;
            captured = true;
            queue.armed();
            if !capture_ready.wait_timeout(Duration::from_secs(5)) {
                return Err("microphone".into());
            }
            // Capture buffers during model loading.
            let began = Instant::now();
            while queue.active() {
                match ready.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ok(())) => {
                        queue.ready();
                        return Ok(());
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("model".into())
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                self.commit_silence(&queue);
                self.publish(&queue);
                if began.elapsed() > Duration::from_secs(60) {
                    return Err("timeout".into());
                }
            }
            Ok(())
        })();
        if let Err(error) = startup {
            queue.fault(&error);
        }
        let mut previous_revision = u64::MAX;
        let mut committing_since: Option<(u64, Instant)> = None;
        while queue.active() {
            self.commit_silence(&queue);
            let snapshot = queue.snapshot();
            if snapshot.revision != previous_revision {
                self.publish(&queue);
                previous_revision = snapshot.revision;
            }
            if snapshot.status == Status::Committing {
                if committing_since.is_none_or(|(phrase, _)| phrase != snapshot.phrase) {
                    committing_since = Some((snapshot.phrase, Instant::now()));
                }
                if committing_since.is_some_and(|(_, began)| began.elapsed() > COMMIT_TIMEOUT) {
                    log::warn!(
                        "Voice phrase {} not delivered within {COMMIT_TIMEOUT:?}",
                        snapshot.phrase
                    );
                    queue.fault("timeout");
                }
            } else {
                committing_since = None;
            }
            if captured && !self.audio.voice_capture_healthy() {
                log::warn!("Voice microphone capture unhealthy");
                queue.fault("microphone");
            }
            if let Some(request) = queue.take_paste() {
                log::info!(
                    "Voice phrase {} ({} chars) delivered to the log (target {})",
                    request.phrase,
                    request.text.chars().count(),
                    request.target.app_id
                );
                log::debug!("Voice phrase {} text: {}", request.phrase, request.text);
                queue.paste_done(request.generation, request.phrase, Ok(()));
            }
            if let Some(request) = queue.take_submit() {
                log::info!("Voice Enter app_id={}: log only", request.target.app_id);
                queue.submit_done(request.generation, Ok(()));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if captured {
            self.audio.cancel_recording();
        }
        self.publish(&queue);
        // No new generation may start until the previous lease is actually free.
        while self.transcription.voice_worker_busy() {
            std::thread::sleep(Duration::from_millis(30));
        }
        queue.stopped();
        self.publish(&queue);
        self.busy.store(false, Ordering::Release);
    }

    fn publish(&self, queue: &VoiceQueue) {
        let snapshot = queue.snapshot();
        log::debug!(
            "Voice state {:?} phrase {} speech {}",
            snapshot.status,
            snapshot.phrase,
            snapshot.speech_active
        );
    }

    fn commit_silence(self: &Arc<Self>, queue: &Arc<VoiceQueue>) {
        let Some(phrase) = queue.reserve_silence_boundary() else {
            return;
        };
        // Marker first: its FIFO position is the moment silence was measured.
        let marker = BoundaryMarker::new(Arc::clone(queue));
        if let Err(error) = self.audio.voice_boundary(Box::new(move || marker.commit())) {
            log::warn!("Voice boundary marker for phrase {phrase} not queued: {error}");
            queue.fault("microphone");
            return;
        }
        queue.set_target(phrase, log_target());
    }

    /// Toggle off: unpasted text survives as a draft.
    pub fn stop(&self) {
        self.stop_session(true);
    }

    fn stop_session(&self, keep_draft: bool) {
        let queue = lock(&self.session).clone();
        if let Some(queue) = queue {
            if queue.snapshot().status == Status::Stopping {
                return;
            }
            self.generation.fetch_add(1, Ordering::AcqRel);
            if !(keep_draft && queue.cancel_keeping_draft()) {
                self.transcription.end_voice_session(&queue);
            }
            if !self.busy.load(Ordering::Acquire) {
                queue.stopped();
            }
            self.publish(&queue);
        }
    }

    pub fn shutdown(&self) {
        self.stop_session(false);
    }

    /// A voice session (or its teardown) is still running.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}
