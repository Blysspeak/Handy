//! Deterministic phrase boundaries and bounded audio/command storage.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const MAX_BACKLOG_SAMPLES: usize = 16_000 * 20;
const SILENCE_SAMPLES: usize = 16_000;
/// Captured non-speech audio after the last paste before one Enter.
const SEND_SILENCE_SAMPLES: usize = 16_000 / 4;
/// Fragments with fewer letters/digits are VAD noise ("Б"), never pasted.
const MIN_PHRASE_CHARS: usize = 3;
const MAX_COMMANDS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub app_id: String,
    pub pid: u32,
    pub output: String,
    #[serde(default)]
    pub output_x: i32,
    #[serde(default)]
    pub output_y: i32,
}

impl Target {
    pub fn valid(&self) -> bool {
        !self.id.is_empty() && !self.app_id.is_empty() && self.pid != 0
    }

    pub fn same_window(&self, other: &Self) -> bool {
        self.id == other.id && self.app_id == other.app_id && self.pid == other.pid
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Loading,
    Listening,
    Committing,
    Draft,
    Error,
    Stopping,
    Off,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub generation: u64,
    pub phrase: u64,
    pub revision: u64,
    pub status: Status,
    pub committed: String,
    pub tentative: String,
    pub draft: String,
    pub error: Option<String>,
    pub speech_active: bool,
    pub silence_remaining_ms: Option<usize>,
    pub send_remaining_ms: Option<usize>,
}

#[derive(Debug)]
pub enum VoiceCommand {
    Feed { phrase: u64, pcm: Vec<f32> },
    Commit { phrase: u64 },
    End,
}

#[derive(Clone, Debug)]
pub struct PasteRequest {
    pub generation: u64,
    pub phrase: u64,
    pub target: Target,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct SubmitRequest {
    pub generation: u64,
    pub target: Target,
}

struct Pending {
    phrase: u64,
    /// Filled by the focus query that follows the recorder marker.
    target: Option<Target>,
    text: Option<String>,
    pasting: bool,
    speech: bool,
    preview: (String, String),
}

struct Inner {
    snapshot: Snapshot,
    active: bool,
    ready: bool,
    boundary_pending: bool,
    /// Focus reply that arrived before the recorder reached the marker.
    boundary_target: Option<Target>,
    boundary_reserved_at: Option<Instant>,
    commands: VecDeque<VoiceCommand>,
    queued_samples: usize,
    input_phrase: u64,
    input_samples: usize,
    input_speech: bool,
    silence_samples: usize,
    input_text: (String, String),
    pending: VecDeque<Pending>,
    /// Window of the last successful paste that no Enter has followed yet.
    batch: Option<Target>,
    /// Captured non-speech samples since the last paste or speech frame.
    send_silence_samples: usize,
}

pub struct VoiceQueue {
    inner: Mutex<Inner>,
    wake: Condvar,
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Terminal paste must never contain an input control or a line separator.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn meaningful_chars(text: &str) -> usize {
    text.chars().filter(|c| c.is_alphanumeric()).count()
}

fn ms(samples: usize) -> usize {
    // Captured 16 kHz audio only; keep the last sub-ms sample visible.
    (samples * 1000).div_ceil(16_000)
}

/// Recorder marker that cannot vanish silently: if the recorder drops it
/// without running it, the phrase would stay reserved forever and its text
/// would never be pasted, so the session stops with the draft preserved.
pub struct BoundaryMarker {
    queue: Arc<VoiceQueue>,
    done: bool,
}

impl BoundaryMarker {
    pub fn new(queue: Arc<VoiceQueue>) -> Self {
        Self { queue, done: false }
    }

    pub fn commit(mut self) {
        self.done = true;
        self.queue.commit_boundary();
    }
}

impl Drop for BoundaryMarker {
    fn drop(&mut self) {
        if !self.done && self.queue.active() {
            log::warn!("Voice boundary marker dropped by recorder before commit");
            self.queue.fault("microphone");
        }
    }
}

impl VoiceQueue {
    pub fn new(generation: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                snapshot: Snapshot {
                    generation,
                    phrase: 1,
                    revision: 0,
                    status: Status::Loading,
                    committed: String::new(),
                    tentative: String::new(),
                    draft: String::new(),
                    error: None,
                    speech_active: false,
                    silence_remaining_ms: None,
                    send_remaining_ms: None,
                },
                active: true,
                ready: false,
                boundary_pending: false,
                boundary_target: None,
                boundary_reserved_at: None,
                commands: VecDeque::new(),
                queued_samples: 0,
                input_phrase: 1,
                input_samples: 0,
                input_speech: false,
                silence_samples: 0,
                input_text: (String::new(), String::new()),
                pending: VecDeque::new(),
                batch: None,
                send_silence_samples: 0,
            }),
            wake: Condvar::new(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = lock(&self.inner);
        let mut snapshot = s.snapshot.clone();
        snapshot.silence_remaining_ms = Self::silence_remaining_ms(&s);
        snapshot.send_remaining_ms = Self::send_remaining_ms(&s);
        snapshot
    }

    fn silence_remaining_ms(s: &Inner) -> Option<usize> {
        if !s.active
            || !s.ready
            || s.snapshot.status != Status::Listening
            || s.snapshot.phrase != s.input_phrase
            || !s.pending.is_empty()
            || s.boundary_pending
            || !s.input_speech
            || s.snapshot.speech_active
            || s.silence_samples == 0
        {
            return None;
        }
        Some(ms(SILENCE_SAMPLES.saturating_sub(s.silence_samples)))
    }

    /// Enter is allowed only when nothing else can still land in this batch.
    fn send_open(s: &Inner) -> bool {
        s.active
            && s.ready
            && s.batch.is_some()
            && s.snapshot.status == Status::Listening
            && s.pending.is_empty()
            && !s.boundary_pending
            && !s.input_speech
            && !s.snapshot.speech_active
    }

    fn send_remaining_ms(s: &Inner) -> Option<usize> {
        Self::send_open(s).then(|| ms(SEND_SILENCE_SAMPLES.saturating_sub(s.send_silence_samples)))
    }

    fn timers(s: &Inner) -> (Option<usize>, Option<usize>) {
        (Self::silence_remaining_ms(s), Self::send_remaining_ms(s))
    }

    pub fn active(&self) -> bool {
        lock(&self.inner).active
    }

    pub fn armed(&self) {
        lock(&self.inner).ready = true;
    }

    pub fn speech_activity(&self, speech: bool) {
        let mut s = lock(&self.inner);
        if !s.active {
            return;
        }
        let previous = Self::timers(&s);
        if speech && !s.snapshot.speech_active {
            if let (_, Some(remaining)) = previous {
                log::info!("Voice send cancelled by speech {remaining} ms before Enter");
            } else if s.silence_samples >= SILENCE_SAMPLES / 2 {
                log::info!(
                    "Voice silence countdown reset by speech after {} ms",
                    ms(s.silence_samples)
                );
            }
        }
        s.input_speech |= speech;
        if speech {
            s.silence_samples = 0;
            s.send_silence_samples = 0;
        }
        let changed = s.snapshot.speech_active != speech;
        s.snapshot.speech_active = speech;
        if changed || Self::timers(&s) != previous {
            s.snapshot.revision += 1;
        }
    }

    /// Called only after keyboard registration, model and microphone readiness.
    pub fn ready(&self) {
        let mut s = lock(&self.inner);
        if s.active {
            s.ready = true;
            if s.pending.is_empty() {
                s.snapshot.status = Status::Listening;
            }
            s.snapshot.revision += 1;
        }
    }

    pub fn feed(&self, pcm: &[f32]) {
        let mut s = lock(&self.inner);
        if !s.active || pcm.is_empty() {
            return;
        }
        if s.queued_samples.saturating_add(pcm.len()) > MAX_BACKLOG_SAMPLES
            || s.commands.len() >= MAX_COMMANDS
        {
            Self::fault_locked(&mut s, "backlog");
            self.wake.notify_all();
            return;
        }
        let previous = Self::timers(&s);
        s.queued_samples += pcm.len();
        s.input_samples += pcm.len();
        // The recorder reports this frame's VAD result BEFORE feeding its PCM.
        if !s.snapshot.speech_active {
            if s.input_speech {
                s.silence_samples = s.silence_samples.saturating_add(pcm.len());
            }
            if s.batch.is_some() {
                s.send_silence_samples = s.send_silence_samples.saturating_add(pcm.len());
            }
        }
        if Self::timers(&s) != previous {
            s.snapshot.revision += 1;
        }
        let phrase = s.input_phrase;
        s.commands.push_back(VoiceCommand::Feed {
            phrase,
            pcm: pcm.to_vec(),
        });
        self.wake.notify_one();
    }

    /// Reserves the automatic boundary for the phrase being captured and
    /// returns its number. The caller must enqueue the recorder marker right
    /// away, so the cut lands where the silence was measured, and then report
    /// the focused window with [`Self::set_target`].
    pub fn reserve_silence_boundary(&self) -> Option<u64> {
        let mut s = lock(&self.inner);
        if !s.active || !s.ready || s.boundary_pending || !Self::silence_due(&s) {
            return None;
        }
        if s.pending.len() >= 2 {
            Self::fault_locked(&mut s, "backlog");
            self.wake.notify_all();
            return None;
        }
        if Self::silence_remaining_ms(&s).is_some() {
            s.snapshot.revision += 1;
        }
        s.boundary_pending = true;
        s.boundary_target = None;
        s.boundary_reserved_at = Some(Instant::now());
        Some(s.input_phrase)
    }

    fn silence_due(s: &Inner) -> bool {
        s.input_speech && !s.snapshot.speech_active && s.silence_samples >= SILENCE_SAMPLES
    }

    /// Runs in the recorder consumer after pre-marker frames and resampler flush.
    /// The reservation already measured one second of silence and is final:
    /// re-validating speech here let a single VAD flicker during the focus
    /// query cancel the commit and restart the countdown in a loop.
    pub fn commit_boundary(&self) {
        let mut s = lock(&self.inner);
        if !s.active || !s.boundary_pending {
            return;
        }
        let previous = Self::timers(&s);
        s.boundary_pending = false;
        let latency = s.boundary_reserved_at.take().map(|at| at.elapsed());
        let target = s.boundary_target.take();
        if s.input_samples == 0 || (!s.pending.is_empty() && !s.input_speech) {
            log::warn!(
                "Voice boundary for phrase {} not committed: no speech audio",
                s.input_phrase
            );
            if Self::timers(&s) != previous {
                s.snapshot.revision += 1;
            }
            return;
        }
        if s.commands.len() >= MAX_COMMANDS {
            Self::fault_locked(&mut s, "backlog");
            self.wake.notify_all();
            return;
        }
        let phrase = s.input_phrase;
        let pending = Pending {
            phrase,
            target,
            text: None,
            pasting: false,
            speech: s.input_speech,
            preview: s.input_text.clone(),
        };
        s.pending.push_back(pending);
        s.commands.push_back(VoiceCommand::Commit { phrase });
        s.input_phrase += 1;
        s.input_samples = 0;
        s.input_speech = false;
        s.silence_samples = 0;
        s.input_text = (String::new(), String::new());
        if s.snapshot.status != Status::Draft {
            s.snapshot.status = Status::Committing;
        }
        s.snapshot.revision += 1;
        log::info!("Voice phrase {phrase} committed {latency:?} after silence boundary");
        self.wake.notify_one();
    }

    /// Focus query result for a reserved boundary; arrives before or after
    /// the recorder reaches the marker.
    pub fn set_target(&self, phrase: u64, target: Target) {
        let mut s = lock(&self.inner);
        if !s.active {
            return;
        }
        if !target.valid() {
            Self::fault_locked(&mut s, "target_unavailable");
            self.wake.notify_all();
            return;
        }
        log::info!("Voice phrase {phrase} target app_id={}", target.app_id);
        if let Some(pending) = s.pending.iter_mut().find(|p| p.phrase == phrase) {
            pending.target.get_or_insert(target);
        } else if s.boundary_pending && s.input_phrase == phrase {
            s.boundary_target = Some(target);
        }
    }

    pub fn recv(&self) -> VoiceCommand {
        let mut s = lock(&self.inner);
        loop {
            if !s.active {
                return VoiceCommand::End;
            }
            if let Some(command) = s.commands.pop_front() {
                if let VoiceCommand::Feed { ref pcm, .. } = command {
                    s.queued_samples -= pcm.len();
                }
                return command;
            }
            s = self
                .wake
                .wait(s)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    pub fn partial(&self, generation: u64, phrase: u64, committed: String, tentative: String) {
        let mut s = lock(&self.inner);
        if !s.active || s.snapshot.generation != generation {
            return;
        }
        if phrase == s.input_phrase {
            s.input_text = (committed.clone(), tentative.clone());
        }
        if let Some(pending) = s.pending.iter_mut().find(|p| p.phrase == phrase) {
            if pending.text.is_some() {
                return;
            }
            pending.preview = (committed.clone(), tentative.clone());
        }
        if phrase == s.snapshot.phrase {
            s.snapshot.committed = committed;
            s.snapshot.tentative = tentative;
            s.snapshot.revision += 1;
        }
    }

    pub fn finalized(&self, generation: u64, phrase: u64, text: String) {
        let mut s = lock(&self.inner);
        if !s.active || s.snapshot.generation != generation {
            return;
        }
        let Some(pending) = s
            .pending
            .iter_mut()
            .find(|p| p.phrase == phrase && p.text.is_none())
        else {
            return;
        };
        let text = sanitize(&text);
        let chars = meaningful_chars(&text);
        let text = if pending.speech && chars >= MIN_PHRASE_CHARS {
            log::info!(
                "Voice phrase {phrase} final: {} chars",
                text.chars().count()
            );
            text
        } else {
            log::info!("Voice phrase {phrase} dropped: {chars} letters/digits");
            String::new()
        };
        pending.text = Some(text.clone());
        if phrase != s.snapshot.phrase {
            return;
        }
        if text.is_empty() {
            Self::advance(&mut s);
        } else {
            s.snapshot.committed = text;
            s.snapshot.tentative.clear();
            s.snapshot.revision += 1;
        }
    }

    fn drop_empty_front(s: &mut Inner) {
        while s
            .pending
            .front()
            .is_some_and(|p| p.text.as_ref().is_some_and(|text| text.trim().is_empty()))
        {
            s.pending.pop_front();
        }
    }

    /// Taking a request is not permission to paste: helper rechecks generation
    /// and the actual window immediately before clipboard access and key input.
    pub fn take_paste(&self) -> Option<PasteRequest> {
        let mut s = lock(&self.inner);
        if !s.active {
            return None;
        }
        Self::drop_empty_front(&mut s);
        let generation = s.snapshot.generation;
        let batch = s.batch.clone();
        let p = s.pending.front_mut()?;
        if p.pasting {
            return None;
        }
        let (Some(text), Some(target)) = (p.text.clone(), p.target.clone()) else {
            return None;
        };
        p.pasting = true;
        let phrase = p.phrase;
        // Phrases of one unsent batch share a single input line.
        let text = if batch.is_some_and(|batch| batch.same_window(&target)) {
            format!(" {}", text.trim_start())
        } else {
            text
        };
        if s.snapshot.status == Status::Draft {
            // The recoverable draft stays in `draft` until a later paste lands.
            s.snapshot.phrase = phrase;
            s.snapshot.committed = text.trim_start().to_string();
            s.snapshot.tentative.clear();
        }
        s.snapshot.status = Status::Committing;
        s.snapshot.revision += 1;
        Some(PasteRequest {
            generation,
            phrase,
            target,
            text,
        })
    }

    pub fn paste_done(&self, generation: u64, phrase: u64, result: Result<(), String>) {
        let mut s = lock(&self.inner);
        if !s.active || s.snapshot.generation != generation {
            return;
        }
        let Some(p) = s
            .pending
            .front_mut()
            .filter(|p| p.phrase == phrase && p.pasting)
        else {
            return;
        };
        match result {
            Ok(()) => {
                let target = p.target.clone();
                Self::advance(&mut s);
                // Countdown to the single Enter restarts after every paste.
                s.batch = target;
                s.send_silence_samples = 0;
            }
            Err(error) if error == "delivery_uncertain" => {
                Self::fault_locked(&mut s, &error);
                self.wake.notify_all();
            }
            Err(error) => {
                // Keep the refused phrase copyable, but keep listening and
                // delivering later phrases instead of blocking until Off.
                let text = p.text.clone().unwrap_or_default();
                s.pending.pop_front();
                Self::drop_empty_front(&mut s);
                // A non-empty draft here is an earlier refusal not yet
                // superseded by a successful paste (advance clears it).
                if s.snapshot.draft.is_empty() {
                    s.snapshot.draft = text;
                } else {
                    s.snapshot.draft = format!("{} {text}", s.snapshot.draft);
                }
                s.snapshot.phrase = phrase;
                s.snapshot.committed = s.snapshot.draft.clone();
                s.snapshot.tentative.clear();
                s.snapshot.error = Some(error);
                s.snapshot.status = Status::Draft;
                s.batch = None;
                s.snapshot.revision += 1;
            }
        }
    }

    /// At most one Enter per batch: taking the request consumes the batch, so
    /// a refused or failed Enter is never retried.
    pub fn take_submit(&self) -> Option<SubmitRequest> {
        let mut s = lock(&self.inner);
        if !Self::send_open(&s) || s.send_silence_samples < SEND_SILENCE_SAMPLES {
            return None;
        }
        let target = s.batch.take()?;
        s.send_silence_samples = 0;
        s.snapshot.revision += 1;
        Some(SubmitRequest {
            generation: s.snapshot.generation,
            target,
        })
    }

    pub fn submit_done(&self, generation: u64, result: Result<(), String>) {
        let mut s = lock(&self.inner);
        if !s.active || s.snapshot.generation != generation {
            return;
        }
        if let Err(error) = result {
            if error == "delivery_uncertain" {
                Self::fault_locked(&mut s, &error);
                self.wake.notify_all();
            }
            // Focus moved / Off / busy: skip silently, pasted text stays put.
        }
    }

    fn advance(s: &mut Inner) {
        s.pending.pop_front();
        Self::drop_empty_front(s);
        s.snapshot.phrase = s.input_phrase;
        s.snapshot.committed = s.input_text.0.clone();
        s.snapshot.tentative = s.input_text.1.clone();
        s.snapshot.draft.clear();
        s.snapshot.error = None;
        s.snapshot.status = Status::Listening;
        if let Some(pending) = s.pending.front() {
            s.snapshot.phrase = pending.phrase;
            s.snapshot.status = Status::Committing;
            s.snapshot.committed = pending
                .text
                .clone()
                .unwrap_or_else(|| pending.preview.0.clone());
            s.snapshot.tentative = if pending.text.is_none() {
                pending.preview.1.clone()
            } else {
                String::new()
            };
        }
        s.snapshot.revision += 1;
    }

    /// Everything the user said that no window has received, in order.
    fn unsent_text(s: &Inner) -> String {
        let mut drafts = Vec::new();
        if s.snapshot.status != Status::Error && !s.snapshot.draft.trim().is_empty() {
            drafts.push(s.snapshot.draft.clone());
        }
        drafts.extend(
            s.pending
                .iter()
                .map(|p| {
                    p.text
                        .clone()
                        .unwrap_or_else(|| format!("{}{}", p.preview.0, p.preview.1))
                })
                .filter(|text| !text.trim().is_empty()),
        );
        let input = format!("{}{}", s.input_text.0, s.input_text.1);
        if !input.trim().is_empty() {
            drafts.push(input);
        }
        drafts.join(" ")
    }

    fn fault_locked(s: &mut Inner, error: &str) {
        let draft = Self::unsent_text(s);
        log::warn!(
            "Voice session stopped: {error}; draft keeps {} chars",
            draft.chars().count()
        );
        s.active = false;
        s.commands.clear();
        s.queued_samples = 0;
        s.snapshot.draft = draft;
        s.snapshot.error = Some(error.to_string());
        s.snapshot.status = Status::Error;
        s.snapshot.revision += 1;
    }

    pub fn fault(&self, error: &str) {
        let mut s = lock(&self.inner);
        if s.active {
            // Helper EOF can arrive before its outstanding paste reply. Once
            // dispatched, transport failure cannot authorize replaying input.
            let error = if error == "desktop" && s.pending.iter().any(|p| p.pasting) {
                "delivery_uncertain"
            } else {
                error
            };
            Self::fault_locked(&mut s, error);
        }
        self.wake.notify_all();
    }

    /// Turning voice mode off must not erase dictated text: if anything is
    /// still unpasted, stop as `cancelled` with a copyable draft instead.
    pub fn cancel_keeping_draft(&self) -> bool {
        let mut s = lock(&self.inner);
        if !s.active || Self::unsent_text(&s).trim().is_empty() {
            return false;
        }
        Self::fault_locked(&mut s, "cancelled");
        self.wake.notify_all();
        true
    }

    pub fn end(&self) {
        let mut s = lock(&self.inner);
        if s.active {
            let unsent = Self::unsent_text(&s).chars().count();
            if unsent > 0 {
                log::warn!("Voice mode closed; discarded {unsent} chars of unpasted text");
            }
        }
        s.active = false;
        s.commands.clear();
        s.queued_samples = 0;
        s.snapshot.status = Status::Stopping;
        s.snapshot.committed.clear();
        s.snapshot.tentative.clear();
        s.snapshot.draft.clear();
        s.snapshot.error = None;
        s.snapshot.revision += 1;
        self.wake.notify_all();
    }

    pub fn stopped(&self) {
        let mut s = lock(&self.inner);
        if s.snapshot.status == Status::Stopping {
            s.snapshot.status = Status::Off;
            s.snapshot.revision += 1;
        }
    }
}

pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;
    fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            app_id: "ghostty".into(),
            pid: 1,
            output: "DP-1".into(),
            output_x: 0,
            output_y: 0,
        }
    }
    fn queue() -> VoiceQueue {
        let q = VoiceQueue::new(7);
        q.ready();
        q
    }
    /// Marker first (recorder FIFO), focus reply second, like production.
    fn boundary(q: &VoiceQueue, target_id: &str) {
        let phrase = q.reserve_silence_boundary().unwrap();
        q.commit_boundary();
        q.set_target(phrase, target(target_id));
    }
    fn automatic_commit(q: &VoiceQueue, target_id: &str) {
        q.speech_activity(true);
        q.speech_activity(false);
        q.feed(&vec![0.; SILENCE_SAMPLES]);
        boundary(q, target_id);
    }

    #[test]
    fn voice_off_cancels_finalize_and_queued_paste_and_stale_events() {
        let q = queue();
        q.feed(&[1.]);
        automatic_commit(&q, "B");
        q.finalized(6, 1, "stale".into());
        assert!(q.take_paste().is_none());
        q.finalized(7, 2, "wrong phrase".into());
        assert!(q.take_paste().is_none());
        q.finalized(7, 1, "one".into());
        assert!(q.take_paste().is_some());
        q.end();
        q.paste_done(7, 1, Ok(()));
        q.finalized(7, 1, "late".into());
        q.partial(7, 1, "late".into(), String::new());
        assert!(q.take_paste().is_none());
        assert!(matches!(q.recv(), VoiceCommand::End));
        assert_eq!(q.snapshot().status, Status::Stopping);
        assert!(q.snapshot().committed.is_empty());
    }

    #[test]
    fn voice_target_mismatch_keeps_draft_copyable_and_later_phrases_still_deliver() {
        let q = queue();
        q.feed(&[1.]);
        automatic_commit(&q, "B");
        q.feed(&[2.]);
        q.finalized(7, 1, "draft".into());
        let request = q.take_paste().unwrap();
        assert!(!request.target.same_window(&target("C")));
        q.paste_done(7, 1, Err("target_changed".into()));
        let snapshot = q.snapshot();
        assert_eq!(
            (snapshot.status, snapshot.phrase, snapshot.draft.as_str()),
            (Status::Draft, 1, "draft")
        );
        assert!(q.take_paste().is_none());
        // Not a dead end: the next phrase is captured and delivered.
        q.speech_activity(true);
        q.feed(&[3.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "C");
        assert_eq!(q.snapshot().draft, "draft");
        q.finalized(7, 2, "next".into());
        let request = q.take_paste().unwrap();
        assert_eq!(
            (request.text.as_str(), request.target.id.as_str()),
            ("next", "C")
        );
        q.paste_done(7, 2, Err("target_changed".into()));
        assert_eq!(q.snapshot().draft, "draft next");
        q.speech_activity(true);
        q.feed(&[4.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "C");
        q.finalized(7, 3, "third".into());
        assert_eq!(q.take_paste().unwrap().text, "third");
        q.paste_done(7, 3, Ok(()));
        assert_eq!(q.snapshot().status, Status::Listening);
        assert!(q.snapshot().draft.is_empty());
    }

    #[test]
    fn voice_overflow_is_terminal_and_keeps_finalized_draft() {
        let q = queue();
        q.feed(&[1.]);
        automatic_commit(&q, "B");
        q.finalized(7, 1, "kept".into());
        q.feed(&vec![0.; MAX_BACKLOG_SAMPLES]);
        assert!(!q.active());
        assert_eq!(q.snapshot().draft, "kept");
        assert_eq!(q.snapshot().status, Status::Error);
        q.feed(&[1.]);
        assert!(matches!(q.recv(), VoiceCommand::End));
        assert!(q.take_paste().is_none());
    }

    #[test]
    fn voice_paste_cannot_contain_terminal_input_controls() {
        assert_eq!(sanitize("a\r\n\t\u{1b}\u{7f}b\u{2028}"), "a     b ");
    }

    #[test]
    fn voice_fault_keeps_all_available_phrases_in_order_without_duplicate_front() {
        let q = queue();
        q.feed(&[1.]);
        automatic_commit(&q, "B");
        q.feed(&[2.]);
        automatic_commit(&q, "B");
        q.finalized(7, 1, "first".into());
        q.finalized(7, 2, "second".into());
        q.partial(7, 3, "third".into(), " tail".into());
        q.feed(&vec![0.; MAX_BACKLOG_SAMPLES]);
        assert_eq!(q.snapshot().draft, "first second third tail");
    }

    fn silence(q: &VoiceQueue, samples: usize) {
        q.speech_activity(false);
        q.feed(&vec![0.; samples]);
    }

    #[test]
    fn voice_auto_send_requires_exactly_16000_silent_samples_after_actual_speech() {
        let q = queue();
        silence(&q, SILENCE_SAMPLES);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        assert!(q.reserve_silence_boundary().is_none());
        q.speech_activity(true);
        q.feed(&[1.]);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        silence(&q, 1);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(1000));
        silence(&q, 7_999);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(500));
        silence(&q, 7_999);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(1));
        assert!(q.reserve_silence_boundary().is_none());
        silence(&q, 1);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(0));
        assert_eq!(q.reserve_silence_boundary(), Some(1));
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        // Focus reply may arrive before the recorder reaches the marker.
        q.set_target(1, target("timer"));
        q.commit_boundary();
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        q.finalized(7, 1, "Диктор отправляй. Отправляй буквально.".into());
        let request = q.take_paste().unwrap();
        assert_eq!(request.text, "Диктор отправляй. Отправляй буквально.");
        assert_eq!(request.target.id, "timer");
        q.paste_done(7, 1, Ok(()));
        assert_eq!(q.snapshot().status, Status::Listening);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        silence(&q, SILENCE_SAMPLES);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        assert!(q.reserve_silence_boundary().is_none());
        assert!(q.take_paste().is_none());
    }

    #[test]
    fn voice_resumed_speech_resets_timer_but_reserved_marker_is_final() {
        let q = queue();
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, 15_999);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(1));
        q.speech_activity(true);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        q.feed(&[2.]);
        silence(&q, 1);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(1000));
        assert!(q.reserve_silence_boundary().is_none());
        silence(&q, 15_999);
        assert_eq!(q.reserve_silence_boundary(), Some(1));
        assert!(q.reserve_silence_boundary().is_none());
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        q.commit_boundary();
        q.commit_boundary();
        // Audio after the marker belongs to the next phrase.
        q.speech_activity(true);
        q.feed(&[3.]);
        q.set_target(1, target("new"));
        q.set_target(1, target("late duplicate"));
        let s = lock(&q.inner);
        assert_eq!(s.input_phrase, 2);
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.pending[0].target.as_ref().unwrap().id, "new");
        let commits: Vec<_> = s
            .commands
            .iter()
            .filter_map(|c| match c {
                VoiceCommand::Commit { phrase } => Some(*phrase),
                _ => None,
            })
            .collect();
        assert_eq!(commits, [1]);
        assert!(matches!(
            s.commands.back(),
            Some(VoiceCommand::Feed { phrase: 2, .. })
        ));
    }

    /// Field regression: a VAD flicker during the KWin focus query cancelled
    /// the reserved commit, the 1 s strip ran again and again, nothing pasted.
    #[test]
    fn voice_speech_frame_during_target_lookup_cannot_cancel_commit() {
        let q = queue();
        q.speech_activity(true);
        q.feed(&[1.]);
        let mut reservations = 0;
        for _ in 0..5 {
            silence(&q, SILENCE_SAMPLES);
            if let Some(phrase) = q.reserve_silence_boundary() {
                reservations += 1;
                // Breath/click frames already queued ahead of the marker.
                q.speech_activity(true);
                q.feed(&[0.; 480]);
                silence(&q, 480);
                q.commit_boundary();
                q.set_target(phrase, target("A"));
            }
        }
        assert_eq!(reservations, 1);
        q.finalized(7, 1, "длинное надиктованное сообщение".into());
        let request = q.take_paste().unwrap();
        assert_eq!(request.text, "длинное надиктованное сообщение");
        q.paste_done(7, 1, Ok(()));
        assert_eq!(q.snapshot().phrase, 2);
    }

    #[test]
    fn voice_paste_waits_for_focus_reply_and_invalid_target_keeps_draft() {
        let q = queue();
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        let phrase = q.reserve_silence_boundary().unwrap();
        q.commit_boundary();
        q.finalized(7, phrase, "готовый текст".into());
        assert!(q.take_paste().is_none());
        assert_eq!(q.snapshot().status, Status::Committing);
        q.set_target(
            phrase,
            Target {
                pid: 0,
                ..target("A")
            },
        );
        assert_eq!(q.snapshot().status, Status::Error);
        assert_eq!(q.snapshot().error.as_deref(), Some("target_unavailable"));
        assert_eq!(q.snapshot().draft, "готовый текст");
    }

    #[test]
    fn voice_dropped_recorder_marker_stops_with_draft_instead_of_hanging() {
        let q = Arc::new(queue());
        q.speech_activity(true);
        q.feed(&[1.]);
        q.partial(7, 1, "долгий текст".into(), " хвост".into());
        silence(&q, SILENCE_SAMPLES);
        assert!(q.reserve_silence_boundary().is_some());
        drop(BoundaryMarker::new(Arc::clone(&q)));
        assert_eq!(q.snapshot().status, Status::Error);
        assert_eq!(q.snapshot().draft, "долгий текст хвост");
        let q = Arc::new(queue());
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        assert!(q.reserve_silence_boundary().is_some());
        BoundaryMarker::new(Arc::clone(&q)).commit();
        assert!(q.active());
        assert_eq!(q.snapshot().status, Status::Committing);
    }

    #[test]
    fn voice_off_with_unpasted_text_keeps_copyable_draft() {
        let q = queue();
        assert!(!q.cancel_keeping_draft());
        q.speech_activity(true);
        q.feed(&[1.]);
        q.partial(7, 1, "длинное сообщение".into(), " без паузы".into());
        assert!(q.cancel_keeping_draft());
        let snapshot = q.snapshot();
        assert_eq!(snapshot.status, Status::Error);
        assert_eq!(snapshot.error.as_deref(), Some("cancelled"));
        assert_eq!(snapshot.draft, "длинное сообщение без паузы");
        assert!(matches!(q.recv(), VoiceCommand::End));
        // Dismiss still closes it for good.
        q.end();
        q.stopped();
        assert_eq!(q.snapshot().status, Status::Off);
    }

    #[test]
    fn voice_automatic_backlog_keeps_next_speech_and_never_commits_silence_twice() {
        let q = queue();
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "A");
        silence(&q, SILENCE_SAMPLES);
        assert!(q.reserve_silence_boundary().is_none());
        q.speech_activity(true);
        q.feed(&[2.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "B");
        silence(&q, SILENCE_SAMPLES);
        assert!(q.reserve_silence_boundary().is_none());
        q.speech_activity(true);
        q.feed(&[3.]);
        q.finalized(7, 2, "second".into());
        q.finalized(7, 1, "first".into());
        assert_eq!(q.take_paste().unwrap().text, "first");
        q.paste_done(7, 1, Ok(()));
        assert!(q.reserve_silence_boundary().is_none());
        // Different window than the batch: no joiner space.
        assert_eq!(q.take_paste().unwrap().text, "second");
        q.paste_done(7, 2, Ok(()));
        assert_eq!(q.snapshot().phrase, 3);
        assert_eq!(lock(&q.inner).input_samples, SILENCE_SAMPLES + 1);
        silence(&q, 15_999);
        assert!(q.reserve_silence_boundary().is_none());
        silence(&q, 1);
        assert!(q.reserve_silence_boundary().is_some());
    }

    #[test]
    fn voice_uncertain_delivery_stops_without_retry_and_preserves_available_drafts() {
        for helper_died in [false, true] {
            let q = queue();
            q.feed(&[1.]);
            automatic_commit(&q, "B");
            q.finalized(7, 1, "possibly sent".into());
            assert!(q.take_paste().is_some());
            q.partial(7, 2, "next".into(), " phrase".into());
            if helper_died {
                q.fault("desktop");
            } else {
                q.paste_done(7, 1, Err("delivery_uncertain".into()));
            }
            assert!(!q.active());
            assert_eq!(q.snapshot().status, Status::Error);
            assert_eq!(q.snapshot().error.as_deref(), Some("delivery_uncertain"));
            assert_eq!(q.snapshot().draft, "possibly sent next phrase");
            assert!(q.take_paste().is_none());
            assert!(q.take_paste().is_none());
        }
    }

    #[test]
    fn voice_native_failure_classification_survives_inflight_paste() {
        let q = queue();
        q.feed(&[1.]);
        automatic_commit(&q, "B");
        q.finalized(7, 1, "draft".into());
        assert!(q.take_paste().is_some());
        q.fault("out_of_memory");
        assert_eq!(q.snapshot().error.as_deref(), Some("out_of_memory"));
        assert!(q.take_paste().is_none());
    }

    #[test]
    fn voice_remaining_revision_follows_capture_not_text_or_inference_backlog() {
        let q = queue();
        q.partial(7, 1, "inferred text".into(), String::new());
        let idle_revision = q.snapshot().revision;
        silence(&q, 8000);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        assert_eq!(q.snapshot().revision, idle_revision);
        q.speech_activity(true);
        q.feed(&[1.]);
        q.speech_activity(false);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        let revision = q.snapshot().revision;
        q.feed(&[0.]);
        assert_eq!(q.snapshot().revision, revision + 1);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(1000));
        q.feed(&[0.]);
        assert_eq!(q.snapshot().revision, revision + 1);
        q.feed(&vec![0.; 7998]);
        assert_eq!(q.snapshot().revision, revision + 2);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(500));
        assert_eq!(q.snapshot().committed, "inferred text");
        while !lock(&q.inner).commands.is_empty() {
            assert!(matches!(q.recv(), VoiceCommand::Feed { .. }));
        }
        assert_eq!(q.snapshot().silence_remaining_ms, Some(500));
        assert_eq!(q.snapshot().revision, revision + 2);
        q.partial(7, 1, "later inference".into(), " partial".into());
        q.finalized(7, 1, "no boundary yet".into());
        assert_eq!(q.snapshot().silence_remaining_ms, Some(500));
        q.feed(&vec![0.; 8000]);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(0));
        let revision = q.snapshot().revision;
        q.feed(&[0.; 32]);
        assert_eq!(q.snapshot().silence_remaining_ms, Some(0));
        assert_eq!(q.snapshot().revision, revision);
    }

    #[test]
    fn voice_remaining_hidden_until_ready_and_after_fault_or_end() {
        for fault in [false, true] {
            let q = VoiceQueue::new(7);
            q.armed();
            q.speech_activity(true);
            q.feed(&[1.]);
            silence(&q, 8000);
            assert_eq!(q.snapshot().silence_remaining_ms, None);
            q.ready();
            assert_eq!(q.snapshot().silence_remaining_ms, Some(500));
            if fault {
                q.fault("microphone");
                assert_eq!(q.snapshot().status, Status::Error);
            } else {
                q.end();
                assert_eq!(q.snapshot().status, Status::Stopping);
            }
            assert_eq!(q.snapshot().silence_remaining_ms, None);
            q.stopped();
            assert_eq!(q.snapshot().silence_remaining_ms, None);
        }
    }

    #[test]
    fn voice_remaining_hidden_for_pending_phrase_and_recoverable_draft() {
        let q = queue();
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, 8000);
        silence(&q, 8000);
        let phrase = q.reserve_silence_boundary().unwrap();
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        q.commit_boundary();
        q.set_target(phrase, target("A"));
        q.speech_activity(true);
        q.feed(&[2.]);
        silence(&q, 8000);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        q.finalized(7, 1, "first".into());
        assert!(q.take_paste().is_some());
        q.paste_done(7, 1, Err("target_changed".into()));
        assert_eq!(q.snapshot().status, Status::Draft);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
        assert!(q.take_paste().is_none());
        assert_eq!(q.snapshot().phrase, 1);
        assert_eq!(q.snapshot().silence_remaining_ms, None);
    }

    /// Speech, 1 s silence, finalize and a successful paste into `target_id`.
    fn pasted_phrase(q: &VoiceQueue, phrase: u64, target_id: &str, text: &str) -> String {
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(q, SILENCE_SAMPLES);
        boundary(q, target_id);
        q.finalized(7, phrase, text.into());
        let request = q.take_paste().unwrap();
        assert_eq!(request.phrase, phrase);
        q.paste_done(7, phrase, Ok(()));
        request.text
    }

    #[test]
    fn voice_submit_after_short_silence_since_last_paste() {
        let q = queue();
        pasted_phrase(&q, 1, "A", "первая фраза");
        assert!(q.take_submit().is_none());
        assert_eq!(q.snapshot().send_remaining_ms, Some(250));
        silence(&q, SEND_SILENCE_SAMPLES - 1);
        assert_eq!(q.snapshot().send_remaining_ms, Some(1));
        assert!(q.take_submit().is_none());
        silence(&q, 1);
        assert_eq!(q.snapshot().send_remaining_ms, Some(0));
        let request = q.take_submit().unwrap();
        assert_eq!((request.generation, request.target.id.as_str()), (7, "A"));
        assert_eq!(q.snapshot().send_remaining_ms, None);
        q.submit_done(7, Ok(()));
        // One Enter per batch: no second Enter without a new paste.
        silence(&q, SEND_SILENCE_SAMPLES * 2);
        assert!(q.take_submit().is_none());
        assert_eq!(q.snapshot().send_remaining_ms, None);
    }

    #[test]
    fn voice_speech_before_enter_cancels_and_two_phrases_share_one_submit() {
        let q = queue();
        assert_eq!(pasted_phrase(&q, 1, "A", "первая"), "первая");
        silence(&q, SEND_SILENCE_SAMPLES / 2);
        assert_eq!(q.snapshot().send_remaining_ms, Some(125));
        q.speech_activity(true);
        assert_eq!(q.snapshot().send_remaining_ms, None);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        // A phrase is still being captured: its boundary, not Enter, is next.
        assert!(q.take_submit().is_none());
        boundary(&q, "A");
        silence(&q, SILENCE_SAMPLES);
        assert!(q.take_submit().is_none());
        q.finalized(7, 2, " вторая".into());
        assert_eq!(q.take_paste().unwrap().text, " вторая");
        q.paste_done(7, 2, Ok(()));
        // Countdown restarted at the second paste.
        silence(&q, SEND_SILENCE_SAMPLES - 1);
        assert!(q.take_submit().is_none());
        silence(&q, 1);
        assert_eq!(q.take_submit().unwrap().target.id, "A");
        silence(&q, SEND_SILENCE_SAMPLES);
        assert!(q.take_submit().is_none());
        // After Enter the next phrase starts a new line without a joiner.
        assert_eq!(pasted_phrase(&q, 3, "A", "третья"), "третья");
    }

    #[test]
    fn voice_short_fragment_is_not_pasted_and_does_not_start_a_batch() {
        let q = queue();
        for (phrase, text) in [(1, "Б"), (2, " a1 "), (3, "…!?")] {
            q.speech_activity(true);
            q.feed(&[1.]);
            silence(&q, SILENCE_SAMPLES);
            boundary(&q, "A");
            q.finalized(7, phrase, text.into());
            assert!(q.take_paste().is_none());
            assert_eq!(q.snapshot().status, Status::Listening);
        }
        silence(&q, SEND_SILENCE_SAMPLES);
        assert!(q.take_submit().is_none());
        assert_eq!(q.snapshot().send_remaining_ms, None);
        // A noise fragment after a real paste neither pastes nor stalls Enter.
        pasted_phrase(&q, 4, "A", "да да");
        pasted_phrase(&q, 5, "A", "нет");
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "A");
        q.finalized(7, 6, "Б".into());
        assert!(q.take_paste().is_none());
        // Its trailing silence already covers the Enter delay.
        assert!(q.take_submit().is_some());
    }

    #[test]
    fn voice_submit_skipped_on_failed_paste_helper_refusal_or_off() {
        let q = queue();
        pasted_phrase(&q, 1, "A", "первая");
        q.speech_activity(true);
        q.feed(&[1.]);
        silence(&q, SILENCE_SAMPLES);
        boundary(&q, "B");
        q.finalized(7, 2, "вторая".into());
        assert_eq!(q.take_paste().unwrap().text, "вторая");
        q.paste_done(7, 2, Err("target_changed".into()));
        silence(&q, SEND_SILENCE_SAMPLES);
        assert!(q.take_submit().is_none());
        assert_eq!(q.snapshot().draft, "вторая");

        let q = queue();
        pasted_phrase(&q, 1, "A", "первая");
        silence(&q, SEND_SILENCE_SAMPLES);
        let request = q.take_submit().unwrap();
        // Helper refused (target moved / generation changed): no retry.
        q.submit_done(request.generation, Err("target_changed".into()));
        assert!(q.active());
        assert!(q.take_submit().is_none());
        pasted_phrase(&q, 2, "A", "вторая");
        silence(&q, SEND_SILENCE_SAMPLES);
        let request = q.take_submit().unwrap();
        q.submit_done(request.generation + 1, Err("delivery_uncertain".into()));
        assert!(q.active());
        q.submit_done(request.generation, Err("delivery_uncertain".into()));
        assert_eq!(q.snapshot().status, Status::Error);

        let q = queue();
        pasted_phrase(&q, 1, "A", "первая");
        q.end();
        silence(&q, SEND_SILENCE_SAMPLES);
        assert!(q.take_submit().is_none());
    }
}
