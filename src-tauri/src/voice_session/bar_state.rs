//! Voice state for the Omarchy bar widget on Hyprland, where no floating
//! panel is shown. The widget watches one small JSON file; it is replaced
//! atomically and only when what the bar shows actually changes.
use super::state::{lock, Snapshot, Status};
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

/// The overlay's own strings; other locales fall back to English there too.
static VOICE_TEXT: Lazy<Vec<(&'static str, Value)>> = Lazy::new(|| {
    [
        (
            "en",
            include_str!("../../../src/i18n/locales/en/translation.json"),
        ),
        (
            "ru",
            include_str!("../../../src/i18n/locales/ru/translation.json"),
        ),
    ]
    .into_iter()
    .filter_map(|(code, json)| {
        let parsed: Value = serde_json::from_str(json).ok()?;
        Some((code, parsed["voice"].clone()))
    })
    .collect()
});

/// Longest draft preview in the tooltip; the full draft is still copied.
const DRAFT_PREVIEW_CHARS: usize = 300;

fn voice_text(language: &str, pointer: &str) -> Option<String> {
    let language = language.split(['-', '_']).next().unwrap_or("en");
    [language, "en"].into_iter().find_map(|wanted| {
        let (_, voice) = VOICE_TEXT.iter().find(|(code, _)| *code == wanted)?;
        voice.pointer(pointer)?.as_str().map(str::to_owned)
    })
}

/// What the bar shows: an icon state, the tooltip's translation key and an
/// unsent draft. Comparing this, not the snapshot, skips countdown ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: &'static str,
    message: Option<String>,
    draft: String,
}

fn shown(snapshot: Option<&Snapshot>) -> Shown {
    let Some(snapshot) = snapshot else {
        return Shown {
            state: "off",
            message: None,
            draft: String::new(),
        };
    };
    let (state, message) = match snapshot.status {
        Status::Off => ("off", None),
        Status::Stopping => ("off", Some("/stopping".to_string())),
        Status::Loading => ("listening", Some("/loading".to_string())),
        Status::Listening if snapshot.speech_active => ("speech", Some("/speech".to_string())),
        Status::Listening => ("listening", Some("/listening".to_string())),
        Status::Committing => ("sending", Some("/committing".to_string())),
        Status::Draft | Status::Error => {
            // Same fallback as the overlay: an unknown code reads as "model".
            let code = snapshot.error.as_deref().unwrap_or("model");
            let pointer = format!("/errors/{code}");
            let known = voice_text("en", &pointer).is_some();
            (
                "error",
                Some(if known {
                    pointer
                } else {
                    "/errors/model".into()
                }),
            )
        }
    };
    let draft = if state == "error" {
        snapshot.draft.clone()
    } else {
        String::new()
    };
    Shown {
        state,
        message,
        draft,
    }
}

#[derive(Serialize)]
struct BarFile<'a> {
    state: &'a str,
    tooltip: String,
    draft: &'a str,
}

fn render<'a>(shown: &'a Shown, language: &str) -> BarFile<'a> {
    let mut tooltip = shown
        .message
        .as_deref()
        .and_then(|pointer| voice_text(language, pointer))
        .unwrap_or_default();
    if !shown.draft.is_empty() {
        let mut preview: String = shown.draft.chars().take(DRAFT_PREVIEW_CHARS).collect();
        if shown.draft.chars().count() > DRAFT_PREVIEW_CHARS {
            preview.push('…');
        }
        let hint = voice_text(language, "/barCopyHint").unwrap_or_default();
        tooltip = format!("{tooltip}\n\n{preview}\n\n{hint}");
    }
    BarFile {
        state: shown.state,
        tooltip,
        draft: &shown.draft,
    }
}

pub struct BarState {
    path: PathBuf,
    last: Mutex<Option<Shown>>,
}

impl BarState {
    /// `$XDG_RUNTIME_DIR/diktor/voice-state.json`; None without a runtime dir.
    pub fn new() -> Option<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
        Some(Self {
            path: PathBuf::from(runtime)
                .join("diktor")
                .join("voice-state.json"),
            last: Mutex::new(None),
        })
    }

    /// `None` is "voice mode off"; written at startup so a crash cannot leave
    /// a stale "listening" behind.
    pub fn show(&self, snapshot: Option<&Snapshot>, language: impl FnOnce() -> String) {
        let shown = shown(snapshot);
        let mut last = lock(&self.last);
        if last.as_ref() == Some(&shown) {
            return;
        }
        match self.write(&render(&shown, &language())) {
            Ok(()) => *last = Some(shown),
            Err(error) => log::warn!("Voice bar state not written: {error}"),
        }
    }

    fn write(&self, file: &BarFile) -> std::io::Result<()> {
        let directory = self.path.parent().ok_or(std::io::ErrorKind::NotFound)?;
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The draft is dictated text: owner-only, like the helper directory.
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(file)?)?;
        std::fs::rename(temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: Status, speech: bool, error: Option<&str>, draft: &str) -> Snapshot {
        Snapshot {
            generation: 1,
            phrase: 1,
            revision: 1,
            status,
            committed: String::new(),
            tentative: String::new(),
            draft: draft.into(),
            error: error.map(str::to_owned),
            speech_active: speech,
            silence_remaining_ms: None,
            send_remaining_ms: None,
        }
    }

    #[test]
    fn voice_bar_states_follow_the_overlay_tones() {
        let state = |status, speech| shown(Some(&snapshot(status, speech, None, ""))).state;
        assert_eq!(shown(None).state, "off");
        assert_eq!(state(Status::Off, false), "off");
        assert_eq!(state(Status::Stopping, false), "off");
        assert_eq!(state(Status::Loading, false), "listening");
        assert_eq!(state(Status::Listening, false), "listening");
        assert_eq!(state(Status::Listening, true), "speech");
        assert_eq!(state(Status::Committing, false), "sending");
        assert_eq!(state(Status::Error, false), "error");
        assert_eq!(state(Status::Draft, false), "error");
    }

    #[test]
    fn voice_bar_tooltip_is_the_overlay_text_in_the_app_language() {
        let listening = shown(Some(&snapshot(Status::Listening, false, None, "")));
        assert_eq!(render(&listening, "ru").tooltip, "Слушаю");
        assert_eq!(render(&listening, "de").tooltip, "Listening");
        let error = shown(Some(&snapshot(
            Status::Error,
            false,
            Some("unsupported_model"),
            "",
        )));
        assert!(render(&error, "ru")
            .tooltip
            .starts_with("Модель не поддерживает"));
        let unknown = shown(Some(&snapshot(Status::Error, false, Some("weird"), "")));
        assert_eq!(unknown.message.as_deref(), Some("/errors/model"));
        assert_eq!(render(&shown(None), "ru").tooltip, "");
    }

    #[test]
    fn voice_bar_draft_is_copyable_and_previewed_only_for_errors() {
        let draft = "д".repeat(DRAFT_PREVIEW_CHARS + 5);
        let error = shown(Some(&snapshot(
            Status::Draft,
            false,
            Some("target_changed"),
            &draft,
        )));
        let file = render(&error, "ru");
        assert_eq!(file.draft, draft);
        assert!(file
            .tooltip
            .contains(&format!("{}…", "д".repeat(DRAFT_PREVIEW_CHARS))));
        assert!(file
            .tooltip
            .ends_with(&voice_text("ru", "/barCopyHint").unwrap()));
        let listening = shown(Some(&snapshot(Status::Listening, false, None, "old")));
        assert!(listening.draft.is_empty());
    }

    #[test]
    fn voice_bar_skips_countdown_ticks_and_writes_real_changes() {
        let directory = std::env::temp_dir().join(format!("diktor-bar-{}", std::process::id()));
        let bar = BarState {
            path: directory.join("voice-state.json"),
            last: Mutex::new(None),
        };
        let mut calls = 0;
        let mut listening = snapshot(Status::Listening, false, None, "");
        bar.show(Some(&listening), || {
            calls += 1;
            "ru".into()
        });
        listening.revision += 1;
        listening.silence_remaining_ms = Some(500);
        bar.show(Some(&listening), || {
            calls += 1;
            "ru".into()
        });
        assert_eq!(calls, 1);
        let written: Value = serde_json::from_slice(&std::fs::read(&bar.path).unwrap()).unwrap();
        assert_eq!(written["state"], "listening");
        bar.show(None, || "ru".into());
        let written: Value = serde_json::from_slice(&std::fs::read(&bar.path).unwrap()).unwrap();
        assert_eq!(written["state"], "off");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
