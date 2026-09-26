//! Voice state for the Omarchy bar widget on Hyprland, where no floating
//! panel is shown. The widget watches one small JSON file; it is replaced
//! atomically and only when what the bar shows actually changes.
use super::journal::Waiting;
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

fn voice_text(language: &str, pointer: &str) -> String {
    let language = language.split(['-', '_']).next().unwrap_or("en");
    [language, "en"]
        .into_iter()
        .find_map(|wanted| {
            let (_, voice) = VOICE_TEXT.iter().find(|(code, _)| *code == wanted)?;
            voice.pointer(pointer)?.as_str().map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Translation pointer for a refusal/stop code; unknown codes read as
/// "model", like the overlay.
fn error_pointer(code: &str) -> String {
    if code == "interrupted" {
        return "/errors/interrupted".into();
    }
    let pointer = format!("/errors/{code}");
    if voice_text("en", &pointer).is_empty() {
        "/errors/model".into()
    } else {
        pointer
    }
}

/// What the bar shows. Comparing this, not the snapshot, skips countdown ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: &'static str,
    message: Option<String>,
    rescue: Vec<Waiting>,
}

/// The icon is the live voice state only; text that was not inserted goes
/// to the rescue popup and never changes what the icon or a click means.
fn shown(snapshot: Option<&Snapshot>, rescue: &[Waiting]) -> Shown {
    let (state, message) = match snapshot.map(|s| (s.status, s.speech_active, s.error.as_deref())) {
        None | Some((Status::Off, ..)) => ("off", None),
        Some((Status::Stopping, ..)) => ("off", Some("/stopping".to_string())),
        Some((Status::Loading, ..)) => ("listening", Some("/loading".to_string())),
        Some((Status::Listening, true, _)) => ("speech", Some("/speech".to_string())),
        Some((Status::Listening, false, _)) => ("listening", Some("/listening".to_string())),
        Some((Status::Committing, ..)) => ("sending", Some("/committing".to_string())),
        // A refused paste keeps listening; its text is in the rescue popup.
        Some((Status::Draft, ..)) => ("listening", Some("/listening".to_string())),
        Some((Status::Error, _, error)) => ("error", Some(error_pointer(error.unwrap_or("model")))),
    };
    Shown {
        state,
        message,
        rescue: rescue.to_vec(),
    }
}

/// The popup the bar widget draws while text waits to be rescued.
#[derive(Serialize)]
struct RescueFile {
    /// Newest phrase id: a new failure reopens a popup the bar had hidden.
    id: String,
    count: usize,
    title: String,
    reason: String,
    /// Every phrase the popup rescues, oldest first; "copy" copies these.
    text: String,
    copy: String,
    close: String,
}

#[derive(Serialize)]
struct BarFile<'a> {
    state: &'a str,
    tooltip: String,
    rescue: Option<RescueFile>,
}

fn render<'a>(shown: &'a Shown, language: &str) -> BarFile<'a> {
    let tooltip = shown
        .message
        .as_deref()
        .map(|pointer| voice_text(language, pointer))
        .unwrap_or_default();
    let rescue = shown.rescue.last().map(|newest| RescueFile {
        id: newest.id.clone(),
        count: shown.rescue.len(),
        title: voice_text(language, "/barWaiting")
            .replace("{{count}}", &shown.rescue.len().to_string()),
        reason: voice_text(language, &error_pointer(&newest.reason)),
        text: shown
            .rescue
            .iter()
            .map(|w| w.text.trim())
            .collect::<Vec<_>>()
            .join("\n\n"),
        copy: voice_text(language, "/rescueCopy"),
        close: voice_text(language, "/rescueClose"),
    });
    BarFile {
        state: shown.state,
        tooltip,
        rescue,
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

    /// `snapshot` None is "voice mode off"; written at startup so a crash
    /// cannot leave a stale "listening" behind.
    pub fn show(
        &self,
        snapshot: Option<&Snapshot>,
        rescue: &[Waiting],
        language: impl FnOnce() -> String,
    ) {
        let shown = shown(snapshot, rescue);
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
            // Tooltips quote dictated text: owner-only.
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

    fn snapshot(status: Status, speech: bool, error: Option<&str>) -> Snapshot {
        Snapshot {
            generation: 1,
            phrase: 1,
            revision: 1,
            status,
            committed: String::new(),
            tentative: String::new(),
            draft: String::new(),
            error: error.map(str::to_owned),
            speech_active: speech,
            silence_remaining_ms: None,
            send_remaining_ms: None,
        }
    }

    fn waiting(texts: &[&str]) -> Vec<Waiting> {
        texts
            .iter()
            .enumerate()
            .map(|(n, text)| Waiting {
                id: format!("run-1-{n}"),
                at: n as i64,
                text: text.to_string(),
                reason: "target_changed".into(),
            })
            .collect()
    }

    #[test]
    fn voice_bar_icon_is_the_live_state_even_while_text_waits() {
        let pending = waiting(&["не вставилось"]);
        for rescue in [&[][..], &pending[..]] {
            let state = |status, speech| shown(Some(&snapshot(status, speech, None)), rescue).state;
            assert_eq!(shown(None, rescue).state, "off");
            assert_eq!(state(Status::Off, false), "off");
            assert_eq!(state(Status::Stopping, false), "off");
            assert_eq!(state(Status::Loading, false), "listening");
            assert_eq!(state(Status::Listening, false), "listening");
            assert_eq!(state(Status::Listening, true), "speech");
            assert_eq!(state(Status::Committing, false), "sending");
            assert_eq!(state(Status::Draft, false), "listening");
            assert_eq!(state(Status::Error, false), "error");
        }
    }

    #[test]
    fn voice_bar_rescue_popup_carries_every_waiting_phrase_and_the_newest_reason() {
        assert!(render(&shown(None, &[]), "ru").rescue.is_none());
        let mut list = waiting(&["первая", "вторая"]);
        list[1].reason = "interrupted".into();
        let all = shown(Some(&snapshot(Status::Listening, true, None)), &list);
        let file = render(&all, "ru");
        assert_eq!(file.state, "speech");
        assert_eq!(file.tooltip, "Слышу речь…");
        let rescue = file.rescue.unwrap();
        assert_eq!(rescue.id, "run-1-1");
        assert_eq!(rescue.count, 2);
        assert_eq!(rescue.title, "Не вставлено: 2");
        assert_eq!(rescue.reason, voice_text("ru", "/errors/interrupted"));
        assert_eq!(rescue.text, "первая\n\nвторая");
        assert_eq!(rescue.copy, "Скопировать");
        assert_eq!(rescue.close, "Закрыть");
    }

    #[test]
    fn voice_bar_tooltip_is_the_overlay_text_in_the_app_language() {
        let listening = shown(Some(&snapshot(Status::Listening, false, None)), &[]);
        assert_eq!(render(&listening, "ru").tooltip, "Слушаю");
        assert_eq!(render(&listening, "de").tooltip, "Listening");
        let error = shown(
            Some(&snapshot(Status::Error, false, Some("unsupported_model"))),
            &[],
        );
        assert!(render(&error, "ru")
            .tooltip
            .starts_with("Модель не поддерживает"));
        let unknown = shown(Some(&snapshot(Status::Error, false, Some("weird"))), &[]);
        assert_eq!(unknown.message.as_deref(), Some("/errors/model"));
        assert_eq!(render(&shown(None, &[]), "ru").tooltip, "");
    }

    #[test]
    fn voice_bar_skips_countdown_ticks_and_writes_real_changes() {
        let directory = std::env::temp_dir().join(format!("diktor-bar-{}", std::process::id()));
        let bar = BarState {
            path: directory.join("voice-state.json"),
            last: Mutex::new(None),
        };
        let mut calls = 0;
        let mut listening = snapshot(Status::Listening, false, None);
        bar.show(Some(&listening), &[], || {
            calls += 1;
            "ru".into()
        });
        listening.revision += 1;
        listening.silence_remaining_ms = Some(500);
        bar.show(Some(&listening), &[], || {
            calls += 1;
            "ru".into()
        });
        assert_eq!(calls, 1);
        let read =
            || -> Value { serde_json::from_slice(&std::fs::read(&bar.path).unwrap()).unwrap() };
        assert_eq!(read()["state"], "listening");
        assert!(read()["rescue"].is_null());
        bar.show(Some(&listening), &waiting(&["текст"]), || "ru".into());
        assert_eq!(read()["state"], "listening");
        assert_eq!(read()["rescue"]["text"], "текст");
        bar.show(None, &[], || "ru".into());
        assert_eq!(read()["state"], "off");
        assert!(read()["rescue"].is_null());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
