//! `~/.config/diktor/config.toml` (see `contrib/config.example.toml`) and the
//! XDG locations Diktor uses. Every key is optional; the defaults are the
//! owner's working setup, so a missing file is not an error.
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Model file name (id) the owner dictates with.
const DEFAULT_MODEL: &str = "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Path to a streaming GGUF model, or a bare file name/id looked up in
    /// `model_dirs()`.
    pub model: String,
    /// Recognition language as the model names it (`ru`, `en`, ...) or
    /// `auto` (the model detects it per phrase). `diktor lang` changes it.
    pub language: String,
    /// `default` for the system default input, or a name from `diktor devices`.
    pub input_device: String,
    /// `auto`, `vulkan` or `cpu`.
    pub backend: String,
    /// Silero VAD model; empty = the copy installed next to the binary.
    pub vad_model: String,
    /// Phrase journal retention: days and newest-row count, whichever is smaller.
    pub keep_days: u32,
    pub keep_phrases: u32,
    /// `error`, `warn`, `info`, `debug` (debug also logs recognised text).
    pub log_level: String,
    /// Directories whose subdirectory names join the vocabulary.
    pub vocabulary_dirs: Vec<String>,
    /// Subdirectory names too generic to be terms.
    pub vocabulary_skip: Vec<String>,
    /// `auto`: Claude's glossary additions apply at once; `suggest`: they wait
    /// for `diktor glossary approve`.
    pub claude_glossary: String,
    /// `diktor say`: engine, voice, speed.
    pub tts: crate::tts::TtsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            language: "auto".into(),
            input_device: "default".into(),
            backend: "auto".into(),
            vad_model: String::new(),
            keep_days: 30,
            keep_phrases: 1000,
            log_level: "info".into(),
            vocabulary_dirs: vec!["~/workSpace/project".into()],
            vocabulary_skip: vec!["src".into(), "archive".into(), "spec-kit".into()],
            claude_glossary: "auto".into(),
            tts: crate::tts::TtsConfig::default(),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn xdg(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// `$DIKTOR_CONFIG`, else `$XDG_CONFIG_HOME/diktor/config.toml`.
pub fn config_path() -> PathBuf {
    std::env::var_os("DIKTOR_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("diktor/config.toml"))
}

/// `auto`, or a language code like `ru`, `en`, `pt-BR`.
pub fn valid_language(code: &str) -> bool {
    code == "auto"
        || code.split('-').enumerate().all(|(i, part)| {
            (2..=4).contains(&part.len())
                && part.chars().all(|c| c.is_ascii_alphabetic())
                && (i > 0 || part.chars().all(|c| c.is_ascii_lowercase()))
        })
}

/// Writes `key = value` (in `[section]`, or at the top level) into the config
/// file, keeping every other line and comment; creates the file when missing.
/// Atomic, and only if the result still parses as a config.
pub fn save_value(
    path: &Path,
    section: Option<&str>,
    key: &str,
    value: toml::Value,
) -> anyhow::Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(anyhow::anyhow!("{}: {error}", path.display())),
    };
    let line = format!("{key} = {value}");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = |l: &str| l.trim_start().starts_with('[');
    // The lines of the wanted table: [start, end).
    let (start, end) = match section {
        None => (
            0,
            lines.iter().position(|l| header(l)).unwrap_or(lines.len()),
        ),
        Some(name) => match lines.iter().position(|l| l.trim() == format!("[{name}]")) {
            Some(at) => {
                let end = lines[at + 1..]
                    .iter()
                    .position(|l| header(l))
                    .map_or(lines.len(), |n| at + 1 + n);
                (at + 1, end)
            }
            None => {
                lines.push(String::new());
                lines.push(format!("[{name}]"));
                (lines.len(), lines.len())
            }
        },
    };
    let existing = (start..end).find(|&n| lines[n].split('=').next().unwrap_or("").trim() == key);
    match existing {
        Some(n) => lines[n] = line,
        None => {
            // After the last key of the table, before blank lines/comments of the next.
            let mut at = end;
            while at > start
                && (lines[at - 1].trim().is_empty() || lines[at - 1].trim_start().starts_with('#'))
            {
                at -= 1;
            }
            lines.insert(at, line);
        }
    }
    let updated = lines.join("\n") + "\n";
    let config: Config =
        toml::from_str(&updated).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, updated)?;
    std::fs::rename(temporary, path)?;
    Ok(config)
}

/// `diktor lang`: `language = "<code>"`.
pub fn save_language(path: &Path, code: &str) -> anyhow::Result<()> {
    let config = save_value(
        path,
        None,
        "language",
        toml::Value::String(code.to_string()),
    )?;
    anyhow::ensure!(config.language == code, "language not saved");
    Ok(())
}

/// Phrase journal directory: `$DIKTOR_DATA_DIR`, else `$XDG_DATA_HOME/diktor`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("DIKTOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("diktor"))
}

/// Socket and bar state: `$DIKTOR_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/diktor`
/// (the path the Omarchy bar widget watches).
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    if let Some(directory) = std::env::var_os("DIKTOR_RUNTIME_DIR") {
        return Ok(PathBuf::from(directory));
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(|runtime| PathBuf::from(runtime).join("diktor"))
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is not set"))
}

/// Where a bare model id is looked up: Diktor's own directory first, then the
/// Handy data directory (read only, shared with the CachyOS install).
pub fn model_dirs() -> Vec<PathBuf> {
    let data = xdg("XDG_DATA_HOME", ".local/share");
    vec![
        data.join("diktor/models"),
        data.join("com.pais.handy/models"),
    ]
}

impl Config {
    /// A missing file gives the defaults; a malformed one is an error.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow::anyhow!("{}: {error}", path.display())),
        }
    }

    pub fn model_path(&self) -> anyhow::Result<PathBuf> {
        if self.model.contains('/') {
            let path = expand(&self.model);
            anyhow::ensure!(path.is_file(), "model {} not found", path.display());
            return Ok(path);
        }
        let names = [self.model.clone(), format!("{}.gguf", self.model)];
        model_dirs()
            .iter()
            .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
            .find(|path| path.is_file())
            .ok_or_else(|| {
                anyhow::anyhow!("model {:?} not found in {:?}", self.model, model_dirs())
            })
    }

    pub fn backend(&self) -> anyhow::Result<transcribe_cpp::Backend> {
        use transcribe_cpp::Backend;
        match self.backend.as_str() {
            "auto" => Ok(Backend::Auto),
            "vulkan" => Ok(Backend::Vulkan),
            "cpu" => Ok(Backend::Cpu),
            other => anyhow::bail!("backend {other:?}: expected auto, vulkan or cpu"),
        }
    }

    pub fn input_device(&self) -> Option<String> {
        (self.input_device != "default" && !self.input_device.is_empty())
            .then(|| self.input_device.clone())
    }

    /// Configured path, else `<prefix>/share/diktor/` next to the installed
    /// binary, else the source tree (for `cargo run`).
    pub fn vad_model(&self) -> anyhow::Result<PathBuf> {
        const NAME: &str = "silero_vad_v4.onnx";
        if !self.vad_model.is_empty() {
            return Ok(expand(&self.vad_model));
        }
        let installed = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().and_then(Path::parent).map(Path::to_path_buf))
            .map(|prefix| prefix.join("share/diktor").join(NAME));
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(NAME);
        installed
            .into_iter()
            .chain(std::iter::once(source))
            .find(|path| path.is_file())
            .ok_or_else(|| anyhow::anyhow!("{NAME} not found next to the binary"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_the_owner_setup_and_unknown_keys_are_rejected() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.language, "auto");
        assert_eq!(config.input_device(), None);
        assert!(matches!(
            config.backend(),
            Ok(transcribe_cpp::Backend::Auto)
        ));
        assert!(toml::from_str::<Config>("modle = \"x\"").is_err());
        let config: Config =
            toml::from_str("language = \"en\"\ninput_device = \"pipewire\"\nbackend = \"cpu\"")
                .unwrap();
        assert_eq!(config.language, "en");
        assert_eq!(config.input_device().as_deref(), Some("pipewire"));
        assert!(matches!(config.backend(), Ok(transcribe_cpp::Backend::Cpu)));
        assert!(toml::from_str::<Config>("backend = \"cuda\"")
            .unwrap()
            .backend()
            .is_err());
    }

    #[test]
    fn language_is_saved_in_place_and_validated() {
        let directory = std::env::temp_dir().join(format!("diktor-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("config.toml");
        save_language(&path, "en").unwrap();
        assert_eq!(Config::load(&path).unwrap().language, "en");
        std::fs::write(
            &path,
            "# mine\nmodel = \"x.gguf\"\nlanguage = \"ru\" # note\n",
        )
        .unwrap();
        save_language(&path, "auto").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\nmodel = \"x.gguf\"\n"));
        assert_eq!(Config::load(&path).unwrap().language, "auto");
        assert!(valid_language("ru") && valid_language("auto") && valid_language("pt-BR"));
        assert!(!valid_language("RU") && !valid_language("r") && !valid_language("ru;x"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn values_land_in_their_table_and_keep_comments() {
        let directory =
            std::env::temp_dir().join(format!("diktor-config-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("config.toml");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            &path,
            "# top\nlanguage = \"ru\"\n\n# voice\n[tts]\nvoice = \"bm_fable\"\n",
        )
        .unwrap();
        save_value(&path, Some("tts"), "speed", toml::Value::Float(0.8)).unwrap();
        save_value(
            &path,
            Some("tts"),
            "voice",
            toml::Value::String("am_michael".into()),
        )
        .unwrap();
        save_value(&path, None, "model", toml::Value::String("x.gguf".into())).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            (config.tts.voice.as_str(), config.tts.speed),
            ("am_michael", 0.8)
        );
        assert_eq!(
            (config.model.as_str(), config.language.as_str()),
            ("x.gguf", "ru")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# top\nlanguage = \"ru\"\nmodel = \"x.gguf\"\n\n# voice\n[tts]"));
        // A missing table is appended.
        std::fs::write(&path, "language = \"ru\"\n").unwrap();
        save_value(
            &path,
            Some("tts"),
            "voice",
            toml::Value::String("af_heart".into()),
        )
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().tts.voice, "af_heart");
        // Something that does not parse is not written.
        assert!(save_value(&path, None, "keep_days", toml::Value::String("x".into())).is_err());
        assert_eq!(Config::load(&path).unwrap().tts.voice, "af_heart");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn example_config_parses_to_the_defaults() {
        let example = include_str!("../contrib/config.example.toml");
        let config: Config = toml::from_str(example).unwrap();
        let defaults = Config::default();
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.language, defaults.language);
        assert_eq!(config.input_device, defaults.input_device);
        assert_eq!(config.backend, defaults.backend);
        assert_eq!(config.keep_days, defaults.keep_days);
        assert_eq!(config.keep_phrases, defaults.keep_phrases);
        assert_eq!(config.tts, defaults.tts);
    }
}
