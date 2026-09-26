mod bar_state;
mod desktop;
pub mod state;

use crate::audio::VoiceAudio;
use crate::engine::{voice_error_code, Transcriber};
use crate::journal::{Journal, Phrase};
use bar_state::BarState;
use desktop::DesktopBridge;
use state::{
    lock, BoundaryMarker, PasteRequest, Snapshot, Status, Target, VoiceQueue, COMMIT_TIMEOUT,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A terminal may still be reading the bracketed paste when Enter arrives.
const MIN_PASTE_TO_ENTER: Duration = Duration::from_millis(300);

/// Test safety net: with `DIKTOR_ONLY_WINDOW=<Hyprland window address>` set,
/// every paste and Enter aimed at any other window is refused before the
/// helper is asked (the helper still checks focus itself).
fn only_window_allows(target: &Target) -> Result<(), String> {
    match std::env::var("DIKTOR_ONLY_WINDOW") {
        Ok(only) if only != target.id => {
            log::warn!(
                "Voice delivery to {} refused: DIKTOR_ONLY_WINDOW={only}",
                target.id
            );
            Err("target_changed".into())
        }
        _ => Ok(()),
    }
}

/// Owns persistent voice lifecycle (the Handy fork's `VoiceSessionManager`
/// without the Tauri app and overlay). Model work and desktop IPC never run
/// on the audio thread; stopping invalidates permission to paste synchronously.
pub struct VoiceSessionManager {
    audio: Arc<VoiceAudio>,
    transcription: Arc<Transcriber>,
    desktop: Mutex<Option<Arc<DesktopBridge>>>,
    session: Mutex<Option<Arc<VoiceQueue>>>,
    generation: AtomicU64,
    busy: AtomicBool,
    /// Every recognised phrase, on disk before its paste.
    journal: Arc<Journal>,
    /// The popup's phrases, reloaded whenever the journal changes them.
    rescue: Mutex<Vec<Phrase>>,
    /// Hyprland only: state goes to the Omarchy bar widget.
    bar: OnceLock<BarState>,
    runtime: PathBuf,
}

impl VoiceSessionManager {
    pub fn new(
        audio: Arc<VoiceAudio>,
        transcription: Arc<Transcriber>,
        journal: Arc<Journal>,
        runtime: PathBuf,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            audio,
            transcription,
            desktop: Mutex::new(None),
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            journal,
            rescue: Mutex::new(Vec::new()),
            bar: OnceLock::new(),
            runtime,
        });
        manager.reload_rescue();
        manager
    }

    /// Starts the desktop helper (Hyprland or KDE, from XDG_CURRENT_DESKTOP).
    pub fn initialize(self: &Arc<Self>) {
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
                    let desktop = if bridge.bar_indicator() {
                        "Hyprland"
                    } else {
                        "KDE"
                    };
                    if bridge.bar_indicator() {
                        let _ = manager.bar.set(BarState::new(manager.runtime.clone()));
                        // Also shows text an earlier run could not insert.
                        manager.refresh_bar(None);
                    }
                    *lock(&manager.desktop) = Some(bridge);
                    log::info!("Persistent voice keyboard ready ({desktop}); mode off");
                }
                Err(error) => log::warn!("Persistent voice keyboard unavailable: {error}"),
            }
        });
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        lock(&self.session).as_ref().map(|queue| queue.snapshot())
    }

    pub fn toggle(self: &Arc<Self>) {
        // Toggling either way closes the rescue popup; the text stays waiting.
        self.dismiss_rescue();
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
        // Journaled phrases of this session still waiting for an outcome.
        let mut in_flight = Vec::new();
        while queue.active() {
            self.commit_silence(&queue);
            self.journal_finals(&queue, &mut in_flight);
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
                // A phrase finalised since the loop's first write is on disk
                // before its paste too.
                self.journal_finals(&queue, &mut in_flight);
                if let Some(bridge) = &bridge {
                    let began = Instant::now();
                    let result = if self.generation.load(Ordering::Acquire) == request.generation
                        && queue.active()
                    {
                        only_window_allows(&request.target).and_then(|()| bridge.paste(&request))
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
                    log::debug!("Voice phrase {} text: {}", request.phrase, request.text);
                    if result.is_ok() {
                        last_paste = Instant::now();
                    }
                    self.journal_outcome(&request, &result, &mut in_flight);
                    queue.paste_done(request.generation, request.phrase, result);
                }
            }
            if last_paste.elapsed() >= MIN_PASTE_TO_ENTER {
                if let (Some(bridge), Some(request)) = (&bridge, queue.take_submit()) {
                    let result = if self.generation.load(Ordering::Acquire) == request.generation
                        && queue.active()
                    {
                        only_window_allows(&request.target)
                            .and_then(|()| bridge.submit(request.generation, &request.target))
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
        self.journal_leftovers(&queue, &mut in_flight);
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
        self.refresh_bar(Some(&snapshot));
    }

    /// Every final phrase not yet on disk is written (fsync) now, before
    /// the loop can take its paste.
    fn journal_finals(&self, queue: &VoiceQueue, in_flight: &mut Vec<u64>) {
        for (generation, phrase, text, target) in queue.take_unjournaled() {
            match self
                .journal
                .phrase(generation, phrase, &text, target.as_ref())
            {
                Ok(()) => in_flight.push(phrase),
                Err(error) => log::error!("Voice phrase {phrase} not journaled: {error}"),
            }
        }
    }

    fn journal_outcome(
        &self,
        request: &PasteRequest,
        result: &Result<(), String>,
        in_flight: &mut Vec<u64>,
    ) {
        let before = in_flight.len();
        in_flight.retain(|phrase| *phrase != request.phrase);
        if in_flight.len() == before {
            return;
        }
        let (generation, phrase) = (request.generation, request.phrase);
        let written = match result {
            Ok(()) => self.journal.delivered(generation, phrase, &request.target),
            Err(reason) => self
                .journal
                .failed(generation, phrase, reason, Some(&request.target)),
        };
        if let Err(error) = written {
            log::error!("Voice phrase {phrase} outcome not journaled: {error}");
        }
        if result.is_err() {
            self.reload_rescue();
        }
    }

    /// When a session ends nothing dictated may live only in memory: finals
    /// that never reached a paste, speech still being decoded, and phrases
    /// whose paste never reported back are kept as failed.
    fn journal_leftovers(&self, queue: &VoiceQueue, in_flight: &mut Vec<u64>) {
        self.journal_finals(queue, in_flight);
        let snapshot = queue.snapshot();
        let unfinished = queue.unfinished_text();
        if !unfinished.is_empty() {
            // Phrase numbers start at 1, so 0 cannot collide.
            match self
                .journal
                .phrase(snapshot.generation, 0, &unfinished, None)
            {
                Ok(()) => in_flight.push(0),
                Err(error) => log::error!("Voice unfinished text not journaled: {error}"),
            }
        }
        let reason = snapshot.error.unwrap_or_else(|| "cancelled".to_string());
        for phrase in in_flight.drain(..) {
            log::warn!("Voice phrase {phrase} kept undelivered: {reason}");
            if let Err(error) = self
                .journal
                .failed(snapshot.generation, phrase, &reason, None)
            {
                log::error!("Voice phrase {phrase} outcome not journaled: {error}");
            }
        }
        self.reload_rescue();
    }

    /// Re-reads the popup's phrases (after a failure, copy or dismiss).
    pub fn reload_rescue(&self) {
        match self.journal.rescue() {
            Ok(rescue) => *lock(&self.rescue) = rescue,
            Err(error) => log::error!("Voice journal not readable: {error}"),
        }
    }

    /// Hyprland: the bar icon follows the session, the popup the journal.
    pub fn refresh_bar(&self, snapshot: Option<&Snapshot>) {
        if let Some(bar) = self.bar.get() {
            bar.show(snapshot, &lock(&self.rescue));
        }
    }

    /// Closes the rescue popup; its phrases stay waiting for `diktor copy`.
    fn dismiss_rescue(&self) {
        let ids: Vec<i64> = lock(&self.rescue).iter().map(|p| p.id).collect();
        if ids.is_empty() {
            return;
        }
        if let Err(error) = self.journal.dismissed(&ids) {
            log::error!("Voice rescue not dismissed: {error}");
        }
        self.reload_rescue();
        self.refresh_bar(self.snapshot().as_ref());
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

    /// Voice off: unpasted text survives as a copyable draft. No-op when off.
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

    /// Daemon exit: ends the session and stops the helper, which restores
    /// Hyprland's follow_mouse.
    pub fn shutdown(&self) {
        self.stop_session(false);
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.busy.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
        }
        if let Some(bridge) = lock(&self.desktop).clone() {
            bridge.shutdown();
        }
    }
}
