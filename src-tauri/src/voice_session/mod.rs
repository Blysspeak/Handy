mod bar_state;
mod desktop;
pub mod state;

use crate::managers::{
    audio::AudioRecordingManager,
    transcription::{voice_error_code, TranscriptionManager},
};
use bar_state::BarState;
use desktop::DesktopBridge;
use state::{lock, BoundaryMarker, Snapshot, Status, VoiceQueue, COMMIT_TIMEOUT};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// A terminal may still be reading the bracketed paste when Enter arrives.
const MIN_PASTE_TO_ENTER: Duration = Duration::from_millis(300);

/// Owns persistent voice lifecycle. Model work and desktop IPC never run on the
/// UI/audio thread; stopping invalidates permission to paste synchronously.
pub struct VoiceSessionManager {
    app: AppHandle,
    audio: Arc<AudioRecordingManager>,
    transcription: Arc<TranscriptionManager>,
    desktop: Mutex<Option<Arc<DesktopBridge>>>,
    session: Mutex<Option<Arc<VoiceQueue>>>,
    generation: AtomicU64,
    busy: AtomicBool,
    /// Set on Hyprland only: state goes to the Omarchy bar instead of the
    /// floating panel. Unset (KDE) leaves the panel exactly as it was.
    bar: OnceLock<BarState>,
}

impl VoiceSessionManager {
    pub fn new(
        app: AppHandle,
        audio: Arc<AudioRecordingManager>,
        transcription: Arc<TranscriptionManager>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            audio,
            transcription,
            desktop: Mutex::new(None),
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            bar: OnceLock::new(),
        })
    }

    pub fn initialize(self: &Arc<Self>, start: bool) {
        let manager = Arc::clone(self);
        std::thread::spawn(move || {
            let weak = Arc::downgrade(&manager);
            match DesktopBridge::start(move |event, _target| {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                match event.as_str() {
                    "toggle" => manager.toggle(),
                    "failed" => {
                        if let Some(queue) = lock(&manager.session).as_ref() {
                            queue.fault("desktop");
                        }
                    }
                    _ => {}
                }
            }) {
                Ok(bridge) => {
                    if bridge.bar_indicator() {
                        if let Some(bar) = BarState::new() {
                            bar.show(None, String::new);
                            let _ = manager.bar.set(bar);
                        }
                    }
                    *lock(&manager.desktop) = Some(bridge);
                    log::info!("Persistent voice keyboard ready; mode off (Ctrl+Super+D)");
                    if start {
                        manager.toggle();
                    }
                }
                Err(error) => log::warn!("Persistent voice keyboard unavailable: {error}"),
            }
        });
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
        let generation = queue.snapshot().generation;
        let bridge = lock(&self.desktop).clone();
        let mut captured = false;
        let startup = (|| -> Result<(), String> {
            if self.audio.is_recording() || self.transcription.voice_worker_busy() {
                return Err("busy".into());
            }
            let bridge = bridge.as_ref().ok_or("desktop")?;
            if self.bar.get().is_none() {
                crate::overlay::show_voice_overlay(&self.app)?;
            }
            self.publish(&queue);
            bridge.enable(generation)?;
            if !queue.active() {
                return Ok(());
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
            // Capture buffers during model loading, after helper enable ACK.
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
        let mut last_paste = Instant::now();
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
                if let Some(bridge) = &bridge {
                    let began = Instant::now();
                    let result = if self.generation.load(Ordering::Acquire) == request.generation
                        && queue.active()
                    {
                        bridge.paste(&request)
                    } else {
                        Err("cancelled".into())
                    };
                    log::info!(
                        "Voice paste phrase {} ({} chars) app_id={} in {:?}: {result:?}",
                        request.phrase,
                        request.text.chars().count(),
                        request.target.app_id,
                        began.elapsed()
                    );
                    if result.is_ok() {
                        last_paste = Instant::now();
                    }
                    queue.paste_done(request.generation, request.phrase, result);
                }
            }
            if last_paste.elapsed() >= MIN_PASTE_TO_ENTER {
                if let (Some(bridge), Some(request)) = (&bridge, queue.take_submit()) {
                    let result = if self.generation.load(Ordering::Acquire) == request.generation
                        && queue.active()
                    {
                        bridge.submit(request.generation, &request.target)
                    } else {
                        Err("cancelled".into())
                    };
                    log::info!("Voice Enter app_id={}: {result:?}", request.target.app_id);
                    queue.submit_done(request.generation, result);
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        // On errors preserve draft text. Both normal off and faults release keys.
        if let Some(bridge) = &bridge {
            bridge.disable();
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
        let _ = self.app.emit("voice-session-state", &snapshot);
        if let Some(bar) = self.bar.get() {
            bar.show(Some(&snapshot), || {
                crate::settings::get_settings(&self.app).app_language
            });
        }
        if snapshot.status == Status::Off {
            crate::overlay::hide_voice_overlay(&self.app);
        }
    }

    fn commit_silence(self: &Arc<Self>, queue: &Arc<VoiceQueue>) {
        let Some(phrase) = queue.reserve_silence_boundary() else {
            return;
        };
        // Marker first: its FIFO position is the moment silence was measured,
        // so speech during the slow focus query starts the next phrase instead
        // of cancelling this one.
        let marker = BoundaryMarker::new(Arc::clone(queue));
        if let Err(error) = self.audio.voice_boundary(Box::new(move || marker.commit())) {
            log::warn!("Voice boundary marker for phrase {phrase} not queued: {error}");
            queue.fault("microphone");
            return;
        }
        let bridge = lock(&self.desktop).clone();
        let queue = Arc::clone(queue);
        std::thread::spawn(move || {
            let began = Instant::now();
            match bridge
                .ok_or_else(|| "desktop".to_string())
                .and_then(|b| b.target())
            {
                Ok(target) => {
                    log::info!(
                        "Voice phrase {phrase} focus query took {:?}",
                        began.elapsed()
                    );
                    queue.set_target(phrase, target);
                }
                Err(error) => {
                    log::warn!(
                        "Voice phrase {phrase} focus query failed after {:?}: {error}",
                        began.elapsed()
                    );
                    queue.fault("target_unavailable");
                }
            }
        });
    }

    /// Toggle off: unpasted text survives as a copyable draft.
    pub fn stop(&self) {
        self.stop_session(true);
    }

    /// Explicit close from the overlay discards the draft.
    pub fn dismiss(&self) {
        self.stop_session(false);
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
            if let Some(bridge) = lock(&self.desktop).as_ref() {
                if bridge.set_generation(0).is_err() {
                    bridge.kill();
                }
            }
            if !self.busy.load(Ordering::Acquire) {
                queue.stopped();
            }
            self.publish(&queue);
        }
    }

    pub fn copy_draft(&self, generation: u64, phrase: u64) -> Result<(), String> {
        let snapshot = self.snapshot().ok_or("cancelled")?;
        if snapshot.generation != generation
            || snapshot.phrase != phrase
            || !matches!(snapshot.status, Status::Draft | Status::Error)
            || snapshot.draft.is_empty()
        {
            return Err("cancelled".into());
        }
        let bridge = lock(&self.desktop).clone().ok_or("desktop")?;
        bridge
            .request(serde_json::json!({"op":"copy", "text":snapshot.draft}))
            .map(|_| ())
    }

    pub fn shutdown(&self) {
        self.dismiss();
        if let Some(bridge) = lock(&self.desktop).clone() {
            bridge.shutdown();
        }
    }
}
