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

/// Waiting phrases listed in the tooltip, newest last, and their preview length.
const TOOLTIP_PHRASES: usize = 3;
const PREVIEW_CHARS: usize = 80;

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
    let pointer = format!("/errors/{code}");
    if voice_text("en", &pointer).is_empty() {
        "/errors/model".into()
    } else {
        pointer
    }
}

fn preview(text: &str) -> String {
    let text = text.trim();
    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    preview
}

/// What the bar shows. Comparing this, not the snapshot, skips countdown ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: &'static str,
    message: Option<String>,
    waiting: Vec<Waiting>,
}

fn shown(snapshot: Option<&Snapshot>, waiting: &[Waiting]) -> Shown {
    let (state, message) = match snapshot.map(|s| (s.status, s.speech_active, s.error.as_deref())) {
        None | Some((Status::Off, ..)) => ("off", None),
        Some((Status::Stopping, ..)) => ("off", Some("/stopping".to_string())),
        Some((Status::Loading, ..)) => ("listening", Some("/loading".to_string())),
        Some((Status::Listening, true, _)) => ("speech", Some("/speech".to_string())),
        Some((Status::Listening, false, _)) => ("listening", Some("/listening".to_string())),
        Some((Status::Committing, ..)) => ("sending", Some("/committing".to_string())),
        // A refused paste keeps listening; its text is rescued from the journal.
        Some((Status::Draft, ..)) => ("listening", Some("/listening".to_string())),
        Some((Status::Error, _, error)) => ("error", Some(error_pointer(error.unwrap_or("model")))),
    };
    Shown {
        // Unsaved text outranks everything: the click must rescue it.
        state: if waiting.is_empty() { state } else { "waiting" },
        message,
        waiting: waiting.to_vec(),
    }
}

#[derive(Serialize)]
struct BarFile<'a> {
    state: &'a str,
    tooltip: String,
}

fn render<'a>(shown: &'a Shown, language: &str) -> BarFile<'a> {
    let mut lines = Vec::new();
    if let Some(pointer) = &shown.message {
        lines.push(voice_text(language, pointer));
    }
    if !shown.waiting.is_empty() {
        let count = voice_text(language, "/barWaiting")
            .replace("{{count}}", &shown.waiting.len().to_string());
        let skip = shown.waiting.len().saturating_sub(TOOLTIP_PHRASES);
        let mut block = vec![count];
        block.extend(
            shown.waiting[skip..]
                .iter()
                .map(|w| format!("«{}»", preview(&w.text))),
        );
        lines.push(block.join("\n"));
        lines.push(voice_text(language, "/barWaitingHint"));
    }
    BarFile {
        state: shown.state,
        tooltip: lines.join("\n\n"),
    }
}

/// Desktop notification for text that could not be inserted: summary, body
/// and the copy action's label.
pub fn notice(language: &str, waiting: &[Waiting]) -> Option<(String, String, String)> {
    let newest = waiting.last()?;
    let reason = if newest.reason == "interrupted" {
        "/errors/interrupted".to_string()
    } else {
        error_pointer(&newest.reason)
    };
    let count =
        voice_text(language, "/barWaiting").replace("{{count}}", &waiting.len().to_string());
    // The daemon renders body markup; dictated "<" or "&" must stay text.
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    Some((
        format!("Diktor · {count}"),
        escape(&format!(
            "{}\n\n«{}»",
            voice_text(language, &reason),
            preview(&newest.text)
        )),
        voice_text(language, "/copy"),
    ))
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
        waiting: &[Waiting],
        language: impl FnOnce() -> String,
    ) {
        let shown = shown(snapshot, waiting);
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
    fn voice_bar_states_follow_the_overlay_tones() {
        let state = |status, speech| shown(Some(&snapshot(status, speech, None)), &[]).state;
        assert_eq!(shown(None, &[]).state, "off");
        assert_eq!(state(Status::Off, false), "off");
        assert_eq!(state(Status::Stopping, false), "off");
        assert_eq!(state(Status::Loading, false), "listening");
        assert_eq!(state(Status::Listening, false), "listening");
        assert_eq!(state(Status::Listening, true), "speech");
        assert_eq!(state(Status::Committing, false), "sending");
        assert_eq!(state(Status::Draft, false), "listening");
        assert_eq!(state(Status::Error, false), "error");
    }

    #[test]
    fn voice_bar_waiting_text_outranks_every_state_and_lists_the_newest() {
        let texts = ["первая", "вторая", "третья", &"д".repeat(PREVIEW_CHARS + 3)];
        for status in [Status::Off, Status::Listening, Status::Error] {
            assert_eq!(
                shown(Some(&snapshot(status, false, None)), &waiting(&texts)).state,
                "waiting"
            );
        }
        let all = shown(None, &waiting(&texts));
        let file = render(&all, "ru");
        assert!(file.tooltip.starts_with("Не вставлено: 4"));
        assert!(!file.tooltip.contains("первая"));
        assert!(file.tooltip.contains("«вторая»"));
        assert!(file
            .tooltip
            .contains(&format!("«{}…»", "д".repeat(PREVIEW_CHARS))));
        assert!(file.tooltip.ends_with(&voice_text("ru", "/barWaitingHint")));
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
    fn voice_bar_notice_names_the_reason_and_quotes_the_newest_text() {
        assert!(notice("ru", &[]).is_none());
        let mut list = waiting(&["первая", "вторая"]);
        let (summary, body, action) = notice("ru", &list).unwrap();
        assert_eq!(summary, "Diktor · Не вставлено: 2");
        assert!(body.starts_with("Вернитесь в выбранное окно"));
        assert!(body.ends_with("«вторая»"));
        assert_eq!(action, voice_text("ru", "/copy"));
        list[1].text = "a <b> & c".into();
        assert!(notice("ru", &list)
            .unwrap()
            .1
            .ends_with("«a &lt;b&gt; &amp; c»"));
        list[1].reason = "interrupted".into();
        assert!(notice("ru", &list)
            .unwrap()
            .1
            .starts_with(&voice_text("ru", "/errors/interrupted")));
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
        bar.show(Some(&listening), &waiting(&["текст"]), || "ru".into());
        assert_eq!(read()["state"], "waiting");
        bar.show(None, &[], || "ru".into());
        assert_eq!(read()["state"], "off");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
