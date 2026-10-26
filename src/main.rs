//! Diktor: real-time voice input as one headless binary. `diktor daemon`
//! (run by the systemd user unit) owns the model, microphone and voice mode;
//! the other subcommands talk to it over a Unix socket.
mod audio;
mod audio_toolkit;
mod cli_terms;
mod config;
mod engine;
mod fuzzy;
mod glossary;
mod ipc;
mod journal;
mod models;
mod vocabulary;
mod voice_session;

use clap::{Parser, Subcommand};
use config::Config;
use journal::Journal;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "diktor", version, about = "Real-time voice input for Hyprland")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (model, microphone, voice mode); started by systemd.
    Daemon {
        /// Config file (default: $DIKTOR_CONFIG or ~/.config/diktor/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Tests: at every voice start play this WAV into the capture
        /// pipeline instead of opening the microphone.
        #[arg(long, value_name = "FILE")]
        input_wav: Option<PathBuf>,
    },
    /// Turn voice mode on or off.
    Toggle,
    /// Turn voice mode off; nothing happens when it is already off.
    Off,
    /// Daemon, model and voice mode state.
    Status,
    /// Copy text no window received to the clipboard: without N, what the
    /// bar popup shows (else everything not copied yet); with N, the last N
    /// undelivered phrases, copied before or not.
    Copy {
        n: Option<usize>,
        /// Copy phrase ID (its correction when there is one), any status.
        #[arg(long, conflicts_with = "n")]
        id: Option<i64>,
    },
    /// Close the bar popup; its phrases stay in the journal for `copy`.
    Dismiss,
    /// Recognition language: ru, en, ... or auto (the model detects it per
    /// phrase). Applies from the next phrase and is saved in the config.
    Lang { language: String },
    /// Personal glossary (spoken form -> written form), applied to every phrase.
    Glossary {
        #[command(subcommand)]
        action: cli_terms::GlossaryAction,
    },
    /// Recognised phrases in diktor.db.
    #[command(visible_alias = "phrase")]
    Phrases {
        #[command(subcommand)]
        action: cli_terms::PhrasesAction,
    },
    /// The owner's vocabulary: manual terms and project directory names.
    Vocab {
        #[command(subcommand)]
        action: cli_terms::VocabAction,
    },
    /// What the glossary and the fuzzy layer make of TEXT (no dictation).
    Rewrite {
        text: String,
        #[arg(long)]
        json: bool,
    },
    /// Latin/all-caps tokens in recognised text that no glossary entry covers.
    Misses {
        #[arg(long, default_value_t = 30)]
        days: u32,
        #[arg(long)]
        json: bool,
    },
    /// Recognition models: installed, catalog, download, switch.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Open, close or toggle the Diktor panel of the bar widget.
    Panel {
        #[arg(value_parser = ["open", "close", "toggle"], default_value = "toggle")]
        action: String,
    },
    /// Input device names accepted by `input_device` in the config.
    Devices,
}

#[derive(Subcommand)]
enum ModelAction {
    /// Installed models and the streaming models of the catalog.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Download a catalog model (id or file name; Q8_0 unless --quant).
    Download {
        model: String,
        #[arg(long)]
        quant: Option<String>,
        /// One JSON progress line per update.
        #[arg(long)]
        json: bool,
    },
    /// Switch to an installed model (file name or path): saved in the config,
    /// loaded by the running daemon (voice mode must be off).
    Use {
        model: String,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !matches!(cli.command, Command::Daemon { .. }) {
        // `diktor glossary list | head` should end quietly like any CLI, not
        // panic on the closed pipe. The daemon keeps SIGPIPE ignored: a client
        // hanging up mid-reply must not kill it.
        // SAFETY: restoring the default disposition before any thread starts.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let result = match cli.command {
        Command::Daemon { config, input_wav } => {
            daemon(config.unwrap_or_else(config::config_path), input_wav)
        }
        Command::Toggle => send("toggle"),
        Command::Off => send("off"),
        Command::Status => status(),
        Command::Copy { n, id } => match id {
            Some(id) => copy_id(id),
            None => copy(n),
        },
        Command::Dismiss => dismiss(),
        Command::Lang { language } => lang(&language),
        Command::Glossary { action } => cli_terms::glossary_command(action),
        Command::Phrases { action } => cli_terms::phrases_command(action),
        Command::Vocab { action } => cli_terms::vocab_command(action),
        Command::Misses { days, json } => cli_terms::misses_command(days, json),
        Command::Rewrite { text, json } => cli_terms::rewrite_command(&text, json),
        Command::Model { action } => model_command(action),
        Command::Panel { action } => panel(&action),
        Command::Devices => devices(),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("diktor: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(level: &str) {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(level);
    if let Ok(filters) = std::env::var("RUST_LOG") {
        builder.parse_filters(&filters);
    }
    // journald stamps every line already.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format_timestamp(None);
    } else {
        builder.format_timestamp_millis();
    }
    builder.init();
}

fn rss_kb() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn daemon(config_path: PathBuf, input_wav: Option<PathBuf>) -> anyhow::Result<ExitCode> {
    let config = Config::load(&config_path)?;
    init_logging(&config.log_level);
    let model_path = config.model_path()?;
    let vad_model = config.vad_model()?;
    let backend = config.backend()?;
    let route = Arc::new(engine::VoiceRoute::default());
    let wav = input_wav
        .as_deref()
        .map(audio::WavInput::load)
        .transpose()?;
    let audio = Arc::new(audio::VoiceAudio::new(
        config.input_device(),
        wav,
        vad_model,
        Arc::clone(&route),
    ));
    // Before anything is bound or loaded: a missing microphone is fatal.
    audio.check_input()?;
    let runtime = config::runtime_dir()?;
    let socket = ipc::socket_path(&runtime);
    // Only one daemon gets past this; the journal below is then ours.
    let listener = ipc::bind(&socket)?;
    log::info!(
        "Diktor {} daemon (pid {}), config {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        config_path.display()
    );
    let data = config::data_dir();
    let journal = Arc::new(Journal::open(&data, config.keep_days, config.keep_phrases)?);
    let interrupted = journal.interrupt_pending()?;
    log::info!(
        "Phrase journal {} ({interrupted} interrupted phrase(s) of an earlier run)",
        data.join("diktor.db").display()
    );

    engine::init_transcribe_backend();
    let transcriber = engine::Transcriber::new(
        model_path.clone(),
        backend,
        config.language.clone(),
        Arc::clone(&route),
    );
    transcriber.load_in_background();
    let glossary = Arc::new(glossary::Glossary::new(Arc::clone(&journal)));
    let terms = config.fuzzy_terms.then(|| {
        vocabulary::Live::new(
            Arc::clone(&journal),
            config
                .vocabulary_dirs
                .iter()
                .map(|d| vocabulary::expand(d))
                .collect(),
            config.vocabulary_skip.clone(),
        )
    });
    let manager = voice_session::VoiceSessionManager::new(
        audio,
        Arc::clone(&transcriber),
        Arc::clone(&journal),
        runtime,
        Some(Arc::new(move |text: &str| {
            let text = glossary.apply(text);
            match &terms {
                Some(terms) => terms.apply(&text),
                None => text,
            }
        })),
    );
    manager.initialize();

    let server_manager = Arc::clone(&manager);
    std::thread::spawn(move || {
        ipc::serve(listener, move |request| {
            let voice = |manager: &voice_session::VoiceSessionManager| {
                manager
                    .snapshot()
                    .map(|snapshot| json!(snapshot.status))
                    .unwrap_or(json!("off"))
            };
            match request {
                "toggle" => {
                    server_manager.toggle();
                    json!({ "ok": true, "voice": voice(&server_manager) })
                }
                "off" => {
                    server_manager.stop();
                    json!({ "ok": true, "voice": voice(&server_manager) })
                }
                model if model.starts_with("model ") => {
                    let path = PathBuf::from(model.trim_start_matches("model ").trim());
                    match transcriber.reload(path) {
                        Ok(()) => json!({ "ok": true }),
                        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                    }
                }
                "status" => json!({
                    "ok": true,
                    "pid": std::process::id(),
                    "voice": voice(&server_manager),
                    "model": transcriber.model_state(),
                    "model_path": transcriber.model_path().display().to_string(),
                    "language": transcriber.language(),
                    "rss_kb": rss_kb(),
                    "waiting": journal.waiting().map(|w| w.len()).ok(),
                    "rescue": journal.rescue().map(|r| r.len()).ok(),
                }),
                lang if lang.starts_with("lang ") => {
                    match transcriber.set_language(lang.trim_start_matches("lang ").trim()) {
                        Ok(()) => {
                            server_manager.refresh_bar(server_manager.snapshot().as_ref());
                            json!({ "ok": true, "language": transcriber.language() })
                        }
                        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
                    }
                }
                // The CLI changed the journal (copy, dismiss): redraw the bar.
                "refresh" => {
                    server_manager.reload_rescue();
                    server_manager.refresh_bar(server_manager.snapshot().as_ref());
                    json!({ "ok": true })
                }
                other => json!({ "ok": false, "error": format!("unknown request {other:?}") }),
            }
        })
    });

    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    if let Some(signal) = signals.forever().next() {
        log::info!("Signal {signal}: stopping");
    }
    manager.shutdown();
    let _ = std::fs::remove_file(&socket);
    Ok(ExitCode::SUCCESS)
}

fn socket() -> anyhow::Result<PathBuf> {
    Ok(ipc::socket_path(&config::runtime_dir()?))
}

fn not_running(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("демон не отвечает ({error}); запуск: systemctl --user start diktor")
}

fn send(request: &str) -> anyhow::Result<ExitCode> {
    let reply = ipc::request(&socket()?, request).map_err(not_running)?;
    println!("{}", reply["voice"].as_str().unwrap_or("?"));
    Ok(exit_code(&reply))
}

fn status() -> anyhow::Result<ExitCode> {
    let reply = match ipc::request(&socket()?, "status") {
        Ok(reply) => reply,
        Err(error) => {
            println!("daemon: not running ({error})");
            return Ok(ExitCode::FAILURE);
        }
    };
    let text = |key: &str| match &reply[key] {
        Value::String(text) => text.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    };
    println!("daemon: running, pid {}", text("pid"));
    println!("voice: {}", text("voice"));
    println!("model: {} ({})", text("model"), text("model_path"));
    println!("language: {}", text("language"));
    println!("rss: {} kB", text("rss_kb"));
    println!(
        "undelivered: {} not copied, {} in the bar popup",
        text("waiting"),
        text("rescue")
    );
    Ok(exit_code(&reply))
}

/// The CLI works on the journal directly, so rescue works with the daemon
/// dead too; then pending rows of the dead run can never be delivered.
fn open_journal() -> anyhow::Result<(Journal, PathBuf)> {
    let config = Config::load(&config::config_path())?;
    let socket = socket()?;
    let journal = Journal::open(&config::data_dir(), config.keep_days, config.keep_phrases)?;
    if ipc::request(&socket, "status").is_err() {
        journal.interrupt_pending()?;
    }
    Ok((journal, socket))
}

fn copy(n: Option<usize>) -> anyhow::Result<ExitCode> {
    let (journal, socket) = open_journal()?;
    let phrases = match n {
        Some(n) => journal.last_undelivered(n)?,
        None => {
            let rescue = journal.rescue()?;
            if rescue.is_empty() {
                journal.waiting()?
            } else {
                rescue
            }
        }
    };
    if phrases.is_empty() {
        eprintln!("diktor: нечего копировать — всё надиктованное вставлено");
        return Ok(ExitCode::FAILURE);
    }
    journal::copy_to_clipboard(&journal::joined(&phrases)).map_err(anyhow::Error::msg)?;
    let ids: Vec<i64> = phrases.iter().map(|p| p.id).collect();
    journal.copied(&ids)?;
    let _ = ipc::request(&socket, "refresh");
    println!("скопировано фраз: {}", phrases.len());
    Ok(ExitCode::SUCCESS)
}

fn dismiss() -> anyhow::Result<ExitCode> {
    let (journal, socket) = open_journal()?;
    let ids: Vec<i64> = journal.rescue()?.iter().map(|p| p.id).collect();
    journal.dismissed(&ids)?;
    let _ = ipc::request(&socket, "refresh");
    Ok(ExitCode::SUCCESS)
}

fn lang(code: &str) -> anyhow::Result<ExitCode> {
    anyhow::ensure!(
        config::valid_language(code),
        "{code:?}: ожидается auto или код языка (ru, en, ...)"
    );
    // The running daemon checks the code against the model before it is saved.
    let applied = match ipc::request(&socket()?, &format!("lang {code}")) {
        Ok(reply) if reply["ok"].as_bool() == Some(true) => true,
        Ok(reply) => anyhow::bail!("{}", reply["error"].as_str().unwrap_or("отказ демона")),
        Err(_) => false,
    };
    let path = config::config_path();
    config::save_language(&path, code)?;
    if applied {
        println!(
            "язык: {code} (со следующей фразы; сохранён в {})",
            path.display()
        );
    } else {
        println!(
            "язык: {code} (демон не запущен; сохранён в {})",
            path.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn copy_id(id: i64) -> anyhow::Result<ExitCode> {
    let (journal, socket) = open_journal()?;
    let text = journal
        .phrase_text(id)?
        .ok_or_else(|| anyhow::anyhow!("нет фразы #{id}"))?;
    journal::copy_to_clipboard(&text).map_err(anyhow::Error::msg)?;
    journal.copied(&[id])?;
    let _ = ipc::request(&socket, "refresh");
    println!("скопировано: #{id}");
    Ok(ExitCode::SUCCESS)
}

fn model_command(action: ModelAction) -> anyhow::Result<ExitCode> {
    let path = config::config_path();
    let config = Config::load(&path)?;
    match action {
        ModelAction::List { json } => {
            let entries = models::list(&config)?;
            if json {
                println!("{}", serde_json::to_string(&entries)?);
                return Ok(ExitCode::SUCCESS);
            }
            for entry in entries {
                let state = if entry.current {
                    "* "
                } else if entry.installed {
                    "+ "
                } else {
                    "  "
                };
                let progress = entry
                    .downloading
                    .map(|bytes| format!("  загрузка {} МБ", bytes / 1_000_000))
                    .unwrap_or_default();
                println!(
                    "{state}{}  {} МБ  {}{progress}",
                    entry.file,
                    entry.size_bytes / 1_000_000,
                    entry.name
                );
            }
        }
        ModelAction::Download { model, quant, json } => {
            let target = models::download(&model, quant.as_deref(), |bytes, total| {
                if json {
                    println!("{}", json!({ "bytes": bytes, "total": total }));
                } else {
                    eprint!("\r{} / {} МБ", bytes / 1_000_000, total / 1_000_000);
                }
            })?;
            if json {
                println!(
                    "{}",
                    json!({ "ok": true, "path": target.display().to_string() })
                );
            } else {
                eprintln!();
                println!("{}", target.display());
            }
        }
        ModelAction::Use { model, json } => {
            let entry = models::list(&config)?
                .into_iter()
                .find(|e| e.installed && (e.file == model || e.path.as_deref() == Some(&model)))
                .ok_or_else(|| anyhow::anyhow!("{model}: не установлена (diktor model list)"))?;
            let file = entry.path.clone().unwrap_or(entry.file.clone());
            // Reload first: a daemon that refuses (voice mode on) keeps the old config.
            let reloaded = match ipc::request(&socket()?, &format!("model {file}")) {
                Ok(reply) if reply["ok"].as_bool() == Some(true) => true,
                Ok(reply) => anyhow::bail!("{}", reply["error"].as_str().unwrap_or("отказ демона")),
                Err(_) => false,
            };
            config::save_value(
                &path,
                None,
                "model",
                toml::Value::String(entry.file.clone()),
            )?;
            if json {
                println!(
                    "{}",
                    json!({ "ok": true, "model": entry.file, "reloading": reloaded })
                );
            } else {
                println!(
                    "модель: {}{}",
                    entry.file,
                    if reloaded {
                        " (загружается)"
                    } else {
                        " (демон не запущен)"
                    }
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The widget watches `<runtime>/panel`: "open" or "closed".
fn panel(action: &str) -> anyhow::Result<ExitCode> {
    let file = config::runtime_dir()?.join("panel");
    let open = std::fs::read_to_string(&file).is_ok_and(|s| s.trim() == "open");
    let next = match action {
        "open" => true,
        "close" => false,
        _ => !open,
    };
    if let Some(directory) = file.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let temporary = file.with_extension("tmp");
    std::fs::write(&temporary, if next { "open\n" } else { "closed\n" })?;
    std::fs::rename(temporary, &file)?;
    println!("{}", if next { "open" } else { "closed" });
    Ok(ExitCode::SUCCESS)
}

fn exit_code(reply: &Value) -> ExitCode {
    if reply["ok"].as_bool() == Some(true) {
        ExitCode::SUCCESS
    } else {
        eprintln!("diktor: {}", reply["error"].as_str().unwrap_or("error"));
        ExitCode::FAILURE
    }
}

fn devices() -> anyhow::Result<ExitCode> {
    for (name, default) in audio::device_names()? {
        println!("{name}{}", if default { "  (default)" } else { "" });
    }
    Ok(ExitCode::SUCCESS)
}
