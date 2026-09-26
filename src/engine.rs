//! Streaming recognition for voice mode: one loaded transcribe-cpp session,
//! leased to one voice session at a time. The phrase loop (traits,
//! `drive_voice_session`, `run_voice_worker`, `voice_language`) is the Handy
//! fork's `managers/transcription.rs` voice path, moved here unchanged; the
//! model slot below replaces Handy's model manager and settings.
use crate::voice_session::state::{lock as voice_lock, VoiceCommand, VoiceQueue};
use anyhow::Result;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use transcribe_cpp::{Backend, Model, ModelOptions, RunOptions, Session, StreamOptions, Task};

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

trait VoicePhraseStream {
    fn feed(&mut self, pcm: &[f32]) -> Result<(String, String)>;
    fn finalize(&mut self) -> Result<String>;
}

trait VoiceEngine {
    fn begin(&mut self) -> Result<Box<dyn VoicePhraseStream + '_>>;
}

pub(crate) fn voice_error_code(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<transcribe_cpp::Error>() {
        Some(transcribe_cpp::Error::OutOfMemory(_)) => "out_of_memory",
        Some(transcribe_cpp::Error::NotImplemented(_) | transcribe_cpp::Error::Unsupported(_)) => {
            "unsupported_model"
        }
        _ => "model",
    }
}

/// Keep readiness connected until the first terminal cause is visible to the
/// startup thread, including errors before the first stream and worker panics.
fn run_voice_worker(
    queue: &VoiceQueue,
    ready: mpsc::Sender<std::result::Result<(), String>>,
    run: impl FnOnce(mpsc::Sender<std::result::Result<(), String>>) -> Result<()>,
) -> Result<()> {
    let outcome = catch_unwind(AssertUnwindSafe(|| run(ready.clone()))).unwrap_or_else(|payload| {
        Err(anyhow::anyhow!(
            "Voice model worker panicked: {}",
            panic_payload_message(payload.as_ref())
        ))
    });
    if let Err(error) = &outcome {
        log::error!("Voice model failed: {error:#}");
        let code = voice_error_code(error);
        queue.fault(code);
        let _ = ready.send(Err(code.into()));
    }
    outcome
}

struct CppVoiceEngine<'a> {
    session: &'a mut Session,
    /// Read at every phrase: `diktor lang` applies from the next phrase on.
    language: &'a Mutex<String>,
    languages: Vec<String>,
}

struct CppVoiceStream<'a>(transcribe_cpp::Stream<'a>);

impl VoicePhraseStream for CppVoiceStream<'_> {
    fn feed(&mut self, pcm: &[f32]) -> Result<(String, String)> {
        self.0.feed(pcm)?;
        let text = self.0.text();
        Ok((text.committed, text.tentative))
    }
    fn finalize(&mut self) -> Result<String> {
        self.0.finalize()?;
        Ok(self.0.text().full)
    }
}

impl VoiceEngine for CppVoiceEngine<'_> {
    fn begin(&mut self) -> Result<Box<dyn VoicePhraseStream + '_>> {
        let requested = voice_lock(self.language).clone();
        let options = RunOptions {
            language: voice_language(&requested, &self.languages)?,
            task: Task::Transcribe,
            ..Default::default()
        };
        Ok(Box::new(CppVoiceStream(
            self.session.stream(&options, &StreamOptions::default())?,
        )))
    }
}

/// One engine lease, many successive phrase streams. Every boundary and frame
/// is read from the same bounded FIFO, including during finalize and paste.
fn drive_voice_session(
    engine: &mut impl VoiceEngine,
    queue: &VoiceQueue,
    ready: mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let generation = queue.snapshot().generation;
    let mut ready = Some(ready);
    let mut phrase = 1;
    while queue.active() {
        let mut stream = engine.begin()?;
        if let Some(tx) = ready.take() {
            let _ = tx.send(Ok(()));
        }
        loop {
            match queue.recv() {
                VoiceCommand::Feed {
                    phrase: received,
                    pcm,
                } => {
                    anyhow::ensure!(received == phrase, "Voice frame has wrong phrase");
                    let (committed, tentative) = stream.feed(&pcm)?;
                    queue.partial(generation, phrase, committed, tentative);
                }
                VoiceCommand::Commit { phrase: received } => {
                    anyhow::ensure!(received == phrase, "Voice boundary has wrong phrase");
                    let text = stream.finalize()?;
                    queue.finalized(generation, phrase, text);
                    phrase += 1;
                    break;
                }
                VoiceCommand::End => return Ok(()),
            }
        }
    }
    Ok(())
}

fn voice_language(requested: &str, languages: &[String]) -> Result<Option<String>> {
    if languages.is_empty() || requested == "auto" {
        return Ok(None);
    }
    languages
        .iter()
        .find(|language| language.eq_ignore_ascii_case(requested))
        .or_else(|| {
            languages.iter().find(|language| {
                language
                    .split(['-', '_'])
                    .next()
                    .is_some_and(|primary| primary.eq_ignore_ascii_case(requested))
            })
        })
        .cloned()
        .map(Some)
        .ok_or_else(|| {
            transcribe_cpp::Error::Unsupported(
                "Selected language is unsupported by this streaming model".into(),
            )
            .into()
        })
}

/// Route from the recorder's frame and voice-activity callbacks to the voice
/// session that owns the microphone (Handy's `StreamRouter`, voice part only).
#[derive(Default)]
pub struct VoiceRoute {
    voice: Mutex<Option<Arc<VoiceQueue>>>,
    /// Echo guard: while `diktor say` plays (and a short tail after), the
    /// model hears silence and the VAD reports no speech.
    muted_until: Mutex<Option<Instant>>,
}

/// Room echo and the output buffer outlast the playback a little.
pub const ECHO_TAIL: Duration = Duration::from_millis(300);

impl VoiceRoute {
    fn muted(&self) -> bool {
        voice_lock(&self.muted_until).is_some_and(|until| Instant::now() < until)
    }

    /// Mutes recognition for at most `cap` (a crashed `say` cannot mute forever).
    pub fn mute(&self, cap: Duration) {
        *voice_lock(&self.muted_until) = Some(Instant::now() + cap);
    }

    /// Playback ended: recognition resumes after the echo tail.
    pub fn unmute(&self) {
        *voice_lock(&self.muted_until) = Some(Instant::now() + ECHO_TAIL);
    }

    pub fn feed(&self, frame: &[f32]) {
        if let Some(voice) = voice_lock(&self.voice).as_ref() {
            if self.muted() {
                // Same timing, no sound: phrase boundaries keep working.
                voice.feed(&vec![0.0; frame.len()]);
            } else {
                voice.feed(frame);
            }
        }
    }

    pub fn voice_activity(&self, speech: bool) {
        if let Some(queue) = voice_lock(&self.voice).as_ref() {
            queue.speech_activity(speech && !self.muted());
        }
    }
}

/// Register transcribe-cpp's native backends (per-ISA CPU, Vulkan) once,
/// before the first model load; ggml diagnostics go to the `log` facade.
pub fn init_transcribe_backend() {
    transcribe_cpp::init_logging();
    match transcribe_cpp::init_backends_default() {
        Ok(()) => {
            let devices = transcribe_cpp::devices();
            log::info!(
                "transcribe-cpp initialized with {} compute device(s): [{}]",
                devices.len(),
                devices
                    .iter()
                    .map(|d| format!("{} ({})", d.name, d.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Err(e) => log::warn!("Failed to initialize transcribe-cpp backends: {e}"),
    }
}

/// The loaded model. `Leased` while a voice session holds the session.
enum Slot {
    Loading,
    Ready(Session),
    Leased,
    /// Load failed; the next voice session retries and reports the real cause.
    Failed(String),
}

pub struct Transcriber {
    model_path: PathBuf,
    backend: Backend,
    /// Model language code or "auto"; changed live by `diktor lang`.
    language: Mutex<String>,
    /// The loaded model's languages, for validating `diktor lang`.
    languages: Mutex<Vec<String>>,
    slot: Mutex<Slot>,
    changed: Condvar,
    route: Arc<VoiceRoute>,
    /// One voice worker at a time; cleared only after the lease is returned.
    worker: Mutex<bool>,
}

impl Transcriber {
    pub fn new(
        model_path: PathBuf,
        backend: Backend,
        language: String,
        route: Arc<VoiceRoute>,
    ) -> Arc<Self> {
        Arc::new(Self {
            model_path,
            backend,
            language: Mutex::new(language),
            languages: Mutex::new(Vec::new()),
            slot: Mutex::new(Slot::Loading),
            changed: Condvar::new(),
            route,
            worker: Mutex::new(false),
        })
    }

    /// Load in the background so the daemon answers the socket at once; a
    /// toggle meanwhile buffers audio until the model is ready.
    pub fn load_in_background(self: &Arc<Self>) {
        let transcriber = Arc::clone(self);
        thread::spawn(move || {
            let slot = match transcriber.load() {
                Ok(session) => Slot::Ready(session),
                Err(error) => {
                    log::error!("Model not loaded: {error:#}");
                    Slot::Failed(format!("{error:#}"))
                }
            };
            *voice_lock(&transcriber.slot) = slot;
            transcriber.changed.notify_all();
        });
    }

    fn load(&self) -> Result<Session> {
        let began = Instant::now();
        log::info!(
            "Loading model {} (backend {:?})",
            self.model_path.display(),
            self.backend
        );
        let model = Model::load_with(
            &self.model_path,
            &ModelOptions {
                backend: self.backend,
                device: None,
            },
        )?;
        let caps = model.capabilities();
        anyhow::ensure!(
            caps.supports_streaming,
            transcribe_cpp::Error::NotImplemented("Model has no live stream".into())
        );
        let session = model.session()?;
        *voice_lock(&self.languages) = caps.languages.clone();
        log::info!(
            "Model loaded in {:?}: arch={} backend={} languages={}",
            began.elapsed(),
            model.arch(),
            model.backend(),
            caps.languages.len()
        );
        Ok(session)
    }

    pub fn language(&self) -> String {
        voice_lock(&self.language).clone()
    }

    /// Takes effect from the next phrase, also inside a running session.
    /// Refused when the loaded model does not know the language.
    pub fn set_language(&self, requested: &str) -> Result<()> {
        let languages = voice_lock(&self.languages).clone();
        if !languages.is_empty() {
            voice_language(requested, &languages)?;
        }
        *voice_lock(&self.language) = requested.to_string();
        log::info!("Voice language set to {requested}");
        Ok(())
    }

    /// "loading", "ready", "in use" or "failed: <cause>".
    pub fn model_state(&self) -> String {
        match &*voice_lock(&self.slot) {
            Slot::Loading => "loading".into(),
            Slot::Ready(_) => "ready".into(),
            Slot::Leased => "in use".into(),
            Slot::Failed(error) => format!("failed: {error}"),
        }
    }

    /// Reserve the model for one voice session; readiness (or the first
    /// terminal cause) arrives on the returned channel.
    pub fn start_voice_session(
        self: &Arc<Self>,
        queue: Arc<VoiceQueue>,
    ) -> Result<mpsc::Receiver<std::result::Result<(), String>>> {
        {
            let mut worker = voice_lock(&self.worker);
            anyhow::ensure!(!*worker, "Previous transcription is still stopping");
            *worker = true;
        }
        *voice_lock(&self.route.voice) = Some(Arc::clone(&queue));
        let (ready_tx, ready_rx) = mpsc::channel();
        let transcriber = Arc::clone(self);
        thread::spawn(move || {
            let _ = run_voice_worker(&queue, ready_tx, |ready| {
                transcriber.run_voice_session(&queue, ready)
            });
            let mut route = voice_lock(&transcriber.route.voice);
            if route
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &queue))
            {
                *route = None;
            }
            drop(route);
            *voice_lock(&transcriber.worker) = false;
        });
        Ok(ready_rx)
    }

    fn run_voice_session(
        &self,
        queue: &VoiceQueue,
        ready: mpsc::Sender<std::result::Result<(), String>>,
    ) -> Result<()> {
        let mut lease = Lease {
            transcriber: self,
            session: None,
        };
        {
            let mut slot = voice_lock(&self.slot);
            while matches!(*slot, Slot::Loading) && queue.active() {
                slot = self
                    .changed
                    .wait_timeout(slot, Duration::from_millis(100))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
            if !queue.active() {
                return Ok(());
            }
            match std::mem::replace(&mut *slot, Slot::Leased) {
                Slot::Ready(session) => lease.session = Some(session),
                Slot::Failed(_) => {
                    // Retry once per toggle: the GPU may have been busy at start.
                    drop(slot);
                    match self.load() {
                        Ok(session) => lease.session = Some(session),
                        Err(error) => {
                            *voice_lock(&self.slot) = Slot::Failed(format!("{error:#}"));
                            return Err(error);
                        }
                    }
                }
                other => {
                    *slot = other;
                    anyhow::bail!("Transcription engine is busy");
                }
            }
        }
        let Some(session) = lease.session.as_mut() else {
            anyhow::bail!("No model loaded");
        };
        let caps = session.model().capabilities();
        // An unsupported language fails here, before readiness.
        voice_language(&self.language(), &caps.languages)?;
        drive_voice_session(
            &mut CppVoiceEngine {
                session,
                language: &self.language,
                languages: caps.languages,
            },
            queue,
            ready,
        )
    }

    pub fn end_voice_session(&self, queue: &VoiceQueue) {
        queue.end();
    }

    pub fn voice_worker_busy(&self) -> bool {
        *voice_lock(&self.worker)
    }
}

/// Returns the session to the slot however the voice session ends, panics
/// included; a failed reload leaves `Failed` so the next toggle retries.
struct Lease<'a> {
    transcriber: &'a Transcriber,
    session: Option<Session>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut slot = voice_lock(&self.transcriber.slot);
        if !matches!(*slot, Slot::Leased) {
            return;
        }
        *slot = match self.session.take() {
            Some(session) => Slot::Ready(session),
            None => Slot::Failed("reload failed".into()),
        };
        self.transcriber.changed.notify_all();
    }
}

#[cfg(test)]
mod voice_pipeline_tests {
    use super::*;
    use crate::voice_session::state::Target;

    struct FakeEngine {
        queue: Arc<VoiceQueue>,
        phrases: Arc<Mutex<Vec<Vec<f32>>>>,
    }
    struct FakeStream<'a> {
        engine: &'a FakeEngine,
        audio: Vec<f32>,
    }
    impl VoiceEngine for FakeEngine {
        fn begin(&mut self) -> Result<Box<dyn VoicePhraseStream + '_>> {
            Ok(Box::new(FakeStream {
                engine: self,
                audio: Vec::new(),
            }))
        }
    }
    impl VoicePhraseStream for FakeStream<'_> {
        fn feed(&mut self, pcm: &[f32]) -> Result<(String, String)> {
            self.audio.extend_from_slice(pcm);
            Ok((String::new(), String::new()))
        }
        fn finalize(&mut self) -> Result<String> {
            let mut phrases = voice_lock(&self.engine.phrases);
            phrases.push(self.audio.clone());
            if phrases.len() == 2 {
                self.engine.queue.end();
            }
            Ok("phrase".into())
        }
    }
    fn target() -> Target {
        Target {
            id: "window".into(),
            app_id: "ghostty".into(),
            pid: 1,
            output: "DP-1".into(),
            output_x: 0,
            output_y: 0,
        }
    }

    fn automatic_boundary(queue: &VoiceQueue) {
        queue.speech_activity(true);
        queue.speech_activity(false);
        queue.feed(&vec![0.; 16_000]);
        let phrase = queue.reserve_silence_boundary().expect("boundary");
        queue.commit_boundary();
        queue.set_target(phrase, target());
    }

    #[test]
    fn voice_real_worker_loop_reopens_stream_without_releasing_engine_or_boundary_audio() {
        let queue = Arc::new(VoiceQueue::new(1));
        queue.ready();
        queue.feed(&[1., 2.]);
        automatic_boundary(&queue);
        queue.feed(&[3., 4.]);
        automatic_boundary(&queue);
        let phrases = Arc::new(Mutex::new(Vec::new()));
        let mut engine = FakeEngine {
            queue: Arc::clone(&queue),
            phrases: Arc::clone(&phrases),
        };
        let (ready, _rx) = mpsc::channel();
        drive_voice_session(&mut engine, &queue, ready).unwrap();
        let phrases = voice_lock(&phrases);
        assert_eq!(phrases.len(), 2);
        assert_eq!(&phrases[0][..2], &[1., 2.]);
        assert_eq!(&phrases[1][..2], &[3., 4.]);
    }

    #[test]
    fn echo_guard_feeds_silence_and_no_speech_while_muted() {
        let queue = Arc::new(VoiceQueue::new(1));
        queue.ready();
        let route = VoiceRoute::default();
        *voice_lock(&route.voice) = Some(Arc::clone(&queue));
        route.mute(Duration::from_secs(5));
        route.voice_activity(true);
        route.feed(&[0.5, 0.5]);
        assert!(!queue.snapshot().speech_active);
        match queue.recv() {
            VoiceCommand::Feed { pcm, .. } => assert_eq!(pcm, [0.0, 0.0]),
            _ => panic!("expected a frame"),
        }
        route.unmute();
        assert!(route.muted(), "echo tail still muted");
        *voice_lock(&route.muted_until) = Some(Instant::now());
        route.voice_activity(true);
        route.feed(&[0.5]);
        assert!(queue.snapshot().speech_active);
        match queue.recv() {
            VoiceCommand::Feed { pcm, .. } => assert_eq!(pcm, [0.5]),
            _ => panic!("expected a frame"),
        }
    }

    #[test]
    fn voice_language_resolves_capabilities_without_model_specific_name() {
        assert_eq!(
            voice_language("ru", &["ru-RU".into(), "en-US".into()]).unwrap(),
            Some("ru-RU".into())
        );
        assert_eq!(voice_language("ru", &[]).unwrap(), None);
        assert_eq!(voice_language("auto", &["ru-RU".into()]).unwrap(), None);
        assert!(voice_language("ru", &["en".into()]).is_err());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum FailurePoint {
        Begin,
        Feed,
        Finalize,
        Reopen,
    }

    struct FailingEngine {
        point: FailurePoint,
        error: Option<anyhow::Error>,
        begins: usize,
        feeds: usize,
    }

    impl VoiceEngine for FailingEngine {
        fn begin(&mut self) -> Result<Box<dyn VoicePhraseStream + '_>> {
            self.begins += 1;
            if self.point == FailurePoint::Begin
                || (self.point == FailurePoint::Reopen && self.begins == 2)
            {
                return Err(self.error.take().unwrap());
            }
            Ok(Box::new(FailingStream(self)))
        }
    }

    struct FailingStream<'a>(&'a mut FailingEngine);

    impl VoicePhraseStream for FailingStream<'_> {
        fn feed(&mut self, _pcm: &[f32]) -> Result<(String, String)> {
            self.0.feeds += 1;
            if self.0.point == FailurePoint::Feed && self.0.feeds == 2 {
                return Err(self.0.error.take().unwrap());
            }
            Ok(("Available ".into(), "draft".into()))
        }

        fn finalize(&mut self) -> Result<String> {
            if self.0.point == FailurePoint::Finalize {
                return Err(self.0.error.take().unwrap());
            }
            Ok("Available draft".into())
        }
    }

    #[test]
    fn voice_startup_fault_is_published_before_readiness_can_disconnect() {
        use transcribe_cpp::Error as NativeError;
        for (error, expected) in [
            (
                anyhow::Error::new(NativeError::OutOfMemory("decoder KV".into()))
                    .context("stream begin"),
                "out_of_memory",
            ),
            (
                NativeError::NotImplemented("no stream".into()).into(),
                "unsupported_model",
            ),
            (
                NativeError::Unsupported("language".into()).into(),
                "unsupported_model",
            ),
            (
                NativeError::Backend("inference failed".into()).into(),
                "model",
            ),
            (
                anyhow::anyhow!("unknown inference error mentioning out of memory"),
                "model",
            ),
        ] {
            let queue = Arc::new(VoiceQueue::new(7));
            let worker_queue = Arc::clone(&queue);
            let (ready, rx) = mpsc::channel();
            let (failed, observed) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let worker = thread::spawn(move || {
                run_voice_worker(&worker_queue, ready, |ready| {
                    let mut engine = FailingEngine {
                        point: FailurePoint::Begin,
                        error: Some(error),
                        begins: 0,
                        feeds: 0,
                    };
                    let outcome = drive_voice_session(&mut engine, &worker_queue, ready);
                    // Hold the exact gap between drive returning and publication.
                    failed.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    outcome
                })
            });
            observed.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
            assert!(queue.active());
            release.send(()).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Err(expected.into())
            );
            assert_eq!(queue.snapshot().error.as_deref(), Some(expected));
            assert!(!queue.active());
            assert!(worker.join().unwrap().is_err());
            assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
            // Startup fallback / capture health callbacks must not mask the cause.
            queue.fault("model");
            queue.fault("microphone");
            queue.stopped();
            assert_eq!(queue.snapshot().error.as_deref(), Some(expected));
        }
    }

    #[test]
    fn voice_midstream_fault_keeps_available_draft_and_stops_paste() {
        for point in [
            FailurePoint::Feed,
            FailurePoint::Finalize,
            FailurePoint::Reopen,
        ] {
            let queue = VoiceQueue::new(1);
            queue.ready();
            queue.feed(&[1.]);
            queue.feed(&[2.]);
            automatic_boundary(&queue);
            let mut engine = FailingEngine {
                point,
                error: Some(transcribe_cpp::Error::OutOfMemory("stream allocation".into()).into()),
                begins: 0,
                feeds: 0,
            };
            let (ready, rx) = mpsc::channel();
            assert!(run_voice_worker(&queue, ready, |ready| {
                drive_voice_session(&mut engine, &queue, ready)
            })
            .is_err());
            assert_eq!(rx.recv().unwrap(), Ok(()));
            assert_eq!(rx.recv().unwrap(), Err("out_of_memory".into()));
            assert_eq!(queue.snapshot().draft, "Available draft");
            assert_eq!(queue.snapshot().error.as_deref(), Some("out_of_memory"));
            assert!(!queue.active());
            assert!(queue.take_paste().is_none());
            queue.fault("model");
            assert_eq!(queue.snapshot().draft, "Available draft");
            assert_eq!(queue.snapshot().error.as_deref(), Some("out_of_memory"));
        }
    }

    #[test]
    fn voice_load_failure_and_panic_publish_before_readiness_disconnect() {
        for panic in [false, true] {
            let queue = VoiceQueue::new(1);
            let (ready, rx) = mpsc::channel();
            assert!(run_voice_worker(&queue, ready, |_ready| {
                assert!(!panic, "native worker panic");
                Err(
                    anyhow::Error::new(transcribe_cpp::Error::OutOfMemory("weights".into()))
                        .context("Failed to load model"),
                )
            })
            .is_err());
            let expected = if panic { "model" } else { "out_of_memory" };
            assert_eq!(rx.recv().unwrap(), Err(expected.into()));
            assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
            assert_eq!(queue.snapshot().error.as_deref(), Some(expected));
        }
    }

    /// Opt-in integration of the production Rust worker with public fixtures.
    /// Does not create Tauri, open a microphone, or access the clipboard.
    #[test]
    #[ignore = "requires DIKTOR_VOICE_MODEL, DIKTOR_VOICE_AUDIO_A/B and EXPECT_A/B"]
    fn voice_native_two_phrases() -> Result<()> {
        let started = Instant::now();
        let model_path = std::env::var("DIKTOR_VOICE_MODEL")?;
        let mut fixtures = Vec::new();
        for suffix in ["A", "B"] {
            let mut reader =
                hound::WavReader::open(std::env::var(format!("DIKTOR_VOICE_AUDIO_{suffix}"))?)?;
            let spec = reader.spec();
            anyhow::ensure!(
                spec.sample_rate == 16_000 && spec.channels == 1 && spec.bits_per_sample == 16,
                "Expected mono PCM16/16kHz fixture"
            );
            let samples = reader
                .samples::<i16>()
                .map(|s| s.map(|s| f32::from(s) / 32768.0))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            fixtures.push(samples);
        }
        let queue = Arc::new(VoiceQueue::new(1));
        let worker_queue = Arc::clone(&queue);
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_voice_worker(&worker_queue, ready_tx, |ready| {
                init_transcribe_backend();
                let model = Model::load_with(
                    model_path,
                    &ModelOptions {
                        backend: Backend::Vulkan,
                        ..Default::default()
                    },
                )?;
                let mut session = model.session()?;
                let caps = model.capabilities();
                anyhow::ensure!(caps.supports_streaming, "Fixture model does not stream");
                eprintln!(
                    "native voice backend={} arch={}",
                    model.backend(),
                    model.arch()
                );
                eprintln!(
                    "native voice requested_language=ru resolved_language={:?}",
                    voice_language("ru", &caps.languages)?
                );
                let language = Mutex::new("ru".to_string());
                drive_voice_session(
                    &mut CppVoiceEngine {
                        session: &mut session,
                        language: &language,
                        languages: caps.languages.clone(),
                    },
                    &worker_queue,
                    ready,
                )
            })
        });
        let outcome = (|| -> Result<()> {
            ready_rx
                .recv_timeout(Duration::from_secs(60))?
                .map_err(anyhow::Error::msg)?;
            queue.ready();
            for audio in fixtures {
                for frame in audio.chunks(480) {
                    queue.feed(frame);
                    anyhow::ensure!(queue.active(), "Native fixture overflowed bounded queue");
                    thread::sleep(Duration::from_millis(10));
                }
                queue.speech_activity(true);
                queue.speech_activity(false);
                queue.feed(&vec![0.; 16_000]);
                let Some(phrase) = queue.reserve_silence_boundary() else {
                    anyhow::bail!("Native phrase boundary refused");
                };
                queue.commit_boundary();
                queue.set_target(phrase, target());
            }
            let mut texts = Vec::new();
            for (phrase, suffix) in [(1, "A"), (2, "B")] {
                let deadline = Instant::now() + Duration::from_secs(30);
                let request = loop {
                    if let Some(request) = queue.take_paste() {
                        break request;
                    }
                    anyhow::ensure!(
                        Instant::now() < deadline && queue.active(),
                        "Native finalize timeout/fault"
                    );
                    thread::sleep(Duration::from_millis(10));
                };
                anyhow::ensure!(request.phrase == phrase, "Native phrase order changed");
                let expected =
                    std::env::var(format!("DIKTOR_VOICE_EXPECT_{suffix}"))?.to_lowercase();
                anyhow::ensure!(
                    request.text.to_lowercase().contains(&expected),
                    "Missing phrase {phrase} anchor: {}",
                    request.text
                );
                eprintln!("native phrase {phrase}: {}", request.text);
                texts.push(request.text);
                queue.paste_done(1, phrase, Ok(()));
            }
            let first_anchor = std::env::var("DIKTOR_VOICE_EXPECT_A")?.to_lowercase();
            anyhow::ensure!(
                !texts[1].to_lowercase().contains(&first_anchor),
                "Old phrase leaked into new stream"
            );
            Ok(())
        })();
        queue.end();
        let worker_result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Native worker panicked"))?;
        worker_result?;
        outcome?;
        eprintln!(
            "native voice End/join/model drop clean in {:?}",
            started.elapsed()
        );
        Ok(())
    }
}
