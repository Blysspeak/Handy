//! Speech output (`diktor say`), for English lessons: text -> WAV through a
//! TTS engine, then played with pw-play. Engines are pluggable: each lists its
//! voices and renders a file. The first one is Kokoro through the owner's
//! existing helper (`say-en --out`), so nothing is ported yet.
//!
//! While diktor speaks, the running daemon mutes recognition (see
//! `engine::VoiceRoute::mute`), so the voice from the speakers is never
//! transcribed and pasted.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    /// Engine name; "kokoro" for now.
    pub engine: String,
    pub voice: String,
    pub speed: f32,
    /// Kokoro: the owner's helper script and its model directory.
    pub kokoro_helper: String,
    pub kokoro_home: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            engine: "kokoro".into(),
            voice: "bm_fable".into(),
            speed: 0.95,
            kokoro_helper: "~/.local/bin/say-en".into(),
            kokoro_home: "~/.local/share/kokoro".into(),
        }
    }
}

/// `--slow`: the helper's practice speed.
pub const SLOW_SPEED: f32 = 0.75;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Voice {
    pub engine: String,
    pub id: String,
    /// Language the engine speaks with this voice, e.g. "en-gb".
    pub language: String,
}

pub trait Engine {
    fn name(&self) -> &'static str;
    fn voices(&self) -> anyhow::Result<Vec<Voice>>;
    /// Renders `text` to a WAV file at `out`; plays nothing.
    fn render(&self, text: &str, voice: &str, speed: f32, out: &Path) -> anyhow::Result<()>;
}

pub struct Kokoro {
    helper: PathBuf,
    home: PathBuf,
}

impl Kokoro {
    pub fn new(config: &TtsConfig) -> Self {
        Self {
            helper: crate::vocabulary::expand(&config.kokoro_helper),
            home: crate::vocabulary::expand(&config.kokoro_home),
        }
    }

    /// Kokoro voice ids start with a language letter: a = American, b = British.
    fn language(voice: &str) -> &'static str {
        if voice.starts_with('b') {
            "en-gb"
        } else {
            "en-us"
        }
    }

    fn render_command(&self, text: &str, voice: &str, speed: f32, out: &Path) -> Command {
        let mut command = Command::new(&self.helper);
        command
            .arg("--voice")
            .arg(voice)
            .arg("--speed")
            .arg(format!("{speed:.2}"))
            .arg("--out")
            .arg(out)
            .arg("--")
            .arg(text);
        command
    }
}

impl Engine for Kokoro {
    fn name(&self) -> &'static str {
        "kokoro"
    }

    fn voices(&self) -> anyhow::Result<Vec<Voice>> {
        let python = self.home.join(".venv/bin/python");
        let output = Command::new(&python)
            .arg("-c")
            .arg(
                "import sys\nfrom kokoro_onnx import Kokoro\n\
                 k = Kokoro(sys.argv[1] + '/kokoro-v1.0.onnx', sys.argv[1] + '/voices-v1.0.bin')\n\
                 print('\\n'.join(sorted(k.get_voices())))",
            )
            .arg(&self.home)
            .output()
            .map_err(|e| anyhow::anyhow!("{}: {e}", python.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "kokoro voices: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            // Only the English voices: the helper speaks en-us / en-gb.
            .filter(|id| id.starts_with(['a', 'b']))
            .map(|id| Voice {
                engine: "kokoro".into(),
                id: id.to_string(),
                language: Self::language(id).into(),
            })
            .collect())
    }

    fn render(&self, text: &str, voice: &str, speed: f32, out: &Path) -> anyhow::Result<()> {
        let output = self
            .render_command(text, voice, speed, out)
            .output()
            .map_err(|e| anyhow::anyhow!("{}: {e}", self.helper.display()))?;
        anyhow::ensure!(
            output.status.success() && out.is_file(),
            "kokoro: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
        );
        Ok(())
    }
}

pub fn engine(config: &TtsConfig) -> anyhow::Result<Box<dyn Engine>> {
    match config.engine.as_str() {
        "kokoro" => Ok(Box::new(Kokoro::new(config))),
        other => anyhow::bail!("tts engine {other:?}: only kokoro is available"),
    }
}

/// "прослушать": an English sentence with 2-3 of the owner's terms, so he
/// hears how a voice says his words. `seed` picks template and terms.
pub fn preview_sentence(terms: &[crate::vocabulary::Term], seed: u64) -> String {
    const TEMPLATES: [&str; 5] = [
        "Let's push the fix to {a} and check {b} before the release.",
        "I opened {a} next to {b} to compare the logs.",
        "Could you rebuild {a} tonight and ping me about {b}?",
        "The {a} dashboard says {b} and {c} are still running.",
        "First merge the branch in {a}, then restart {b}.",
    ];
    // Latin terms read naturally in an English sentence.
    let latin: Vec<&str> = terms
        .iter()
        .map(|t| t.forms.last().map_or(t.term.as_str(), String::as_str))
        .filter(|t| t.is_ascii() && !t.trim().is_empty())
        .collect();
    let pick = |n: u64| -> &str {
        if latin.is_empty() {
            "Hyprland"
        } else {
            latin[usize::try_from(n % latin.len() as u64).unwrap_or(0)]
        }
    };
    // A small LCG: different terms per call without a rand dependency.
    let next = |x: u64| {
        x.wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
    };
    let (s1, s2, s3, s4) = (
        next(seed),
        next(next(seed)),
        next(next(next(seed))),
        next(next(next(next(seed)))),
    );
    let template = TEMPLATES[usize::try_from((s1 >> 33) % TEMPLATES.len() as u64).unwrap_or(0)];
    let a = pick(s2 >> 33);
    let mut b = pick(s3 >> 33);
    if b == a && latin.len() > 1 {
        b = pick((s3 >> 33) + 1);
    }
    let mut c = pick(s4 >> 33);
    if (c == a || c == b) && latin.len() > 2 {
        c = pick((s4 >> 33) + 2);
    }
    template
        .replace("{a}", a)
        .replace("{b}", b)
        .replace("{c}", c)
}

/// Length of a rendered WAV, for the recognition mute.
pub fn duration(path: &Path) -> anyhow::Result<std::time::Duration> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let frames = reader.duration();
    Ok(std::time::Duration::from_secs_f64(
        f64::from(frames) / f64::from(spec.sample_rate.max(1)),
    ))
}

/// Plays through the default output and waits until it finished.
pub fn play(path: &Path) -> anyhow::Result<()> {
    let status = Command::new("pw-play")
        .arg(path)
        .status()
        .map_err(|e| anyhow::anyhow!("pw-play: {e}"))?;
    anyhow::ensure!(status.success(), "pw-play: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kokoro_renders_through_the_helper_with_voice_speed_and_file() {
        let kokoro = Kokoro {
            helper: "/opt/say-en".into(),
            home: "/opt/kokoro".into(),
        };
        let command =
            kokoro.render_command("--not an option", "bm_fable", 0.75, Path::new("/t/x.wav"));
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), "/opt/say-en");
        assert_eq!(
            args,
            [
                "--voice",
                "bm_fable",
                "--speed",
                "0.75",
                "--out",
                "/t/x.wav",
                "--",
                "--not an option"
            ]
        );
        assert_eq!(Kokoro::language("bm_fable"), "en-gb");
        assert_eq!(Kokoro::language("af_heart"), "en-us");
    }

    #[test]
    fn preview_uses_the_owners_latin_terms_and_varies() {
        let term = |t: &str| crate::vocabulary::Term {
            term: t.into(),
            forms: vec![t.into()],
            source: "owner".into(),
        };
        let terms = [
            term("xhub"),
            term("Hyprland"),
            term("Фейбл"),
            term("aurelius"),
        ];
        let sentences: std::collections::BTreeSet<String> =
            (0..20).map(|seed| preview_sentence(&terms, seed)).collect();
        assert!(sentences.len() > 3, "{sentences:?}");
        for sentence in &sentences {
            assert!(
                !sentence.contains('{') && !sentence.contains("Фейбл"),
                "{sentence}"
            );
            assert!(["xhub", "Hyprland", "aurelius"]
                .iter()
                .any(|t| sentence.contains(t)));
        }
        assert!(preview_sentence(&[], 1).contains("Hyprland"));
    }

    #[test]
    fn config_defaults_to_the_owners_fable_voice() {
        let config: TtsConfig = toml::from_str("").unwrap();
        assert_eq!(
            (config.engine.as_str(), config.voice.as_str()),
            ("kokoro", "bm_fable")
        );
        assert!(toml::from_str::<TtsConfig>("voise = \"x\"").is_err());
        assert!(engine(&TtsConfig {
            engine: "chatterbox".into(),
            ..TtsConfig::default()
        })
        .is_err());
    }
}
