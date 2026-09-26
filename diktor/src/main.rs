//! Diktor: real-time voice input as one headless binary. `diktor daemon`
//! (run by the systemd user unit) owns the model, microphone and voice mode;
//! the other subcommands talk to it over a Unix socket.
mod audio;
mod audio_toolkit;
mod config;
mod engine;
mod ipc;
mod voice_session;

use clap::{Parser, Subcommand};
use config::Config;
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
    /// Daemon, model and voice mode state.
    Status,
    /// Input device names accepted by `input_device` in the config.
    Devices,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Daemon { config, input_wav } => {
            daemon(config.unwrap_or_else(config::config_path), input_wav)
        }
        Command::Toggle => toggle(),
        Command::Status => status(),
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
    let socket = ipc::socket_path(&config::runtime_dir()?);
    let listener = ipc::bind(&socket)?;
    log::info!(
        "Diktor {} daemon (pid {}), config {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        config_path.display()
    );

    engine::init_transcribe_backend();
    let transcriber =
        engine::Transcriber::new(model_path.clone(), backend, config.language.clone(), route);
    transcriber.load_in_background();
    let manager = voice_session::VoiceSessionManager::new(audio, Arc::clone(&transcriber));
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
                "status" => json!({
                    "ok": true,
                    "pid": std::process::id(),
                    "voice": voice(&server_manager),
                    "model": transcriber.model_state(),
                    "model_path": model_path.display().to_string(),
                    "rss_kb": rss_kb(),
                }),
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

fn toggle() -> anyhow::Result<ExitCode> {
    let reply = ipc::request(&socket()?, "toggle").map_err(not_running)?;
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
    println!("rss: {} kB", text("rss_kb"));
    Ok(exit_code(&reply))
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
