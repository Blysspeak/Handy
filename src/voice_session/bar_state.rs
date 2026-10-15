//! Voice state for the Omarchy bar widget (contrib/omarchy/diktor.qml). The
//! widget watches one small JSON file, replaced atomically and only when what
//! the bar shows changes:
//!
//! `{state, tooltip, language, rescue: null | {id, count, title, reason, text, copy, close}}`
//!
//! `state` is only the live voice state (off / listening / speech / sending /
//! error). `rescue` is text no window received: the widget shows it in a
//! popup with Copy (`diktor copy`) and Close (`diktor dismiss`).
use super::state::{lock, Snapshot, Status};
use crate::journal::{joined, Phrase};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;

/// Tooltip for a voice status.
fn status_text(key: &str) -> &'static str {
    match key {
        "loading" => "Подключаю…",
        "listening" => "Слушаю",
        "speech" => "Слышу речь…",
        "committing" => "Отправляю…",
        "stopping" => "Останавливаю…",
        _ => "",
    }
}

/// Why a phrase or the session stopped; unknown codes read as "model".
pub fn reason_text(code: &str) -> &'static str {
    match code {
        "target_changed" => "Вернитесь в выбранное окно или скопируйте черновик.",
        "target_unavailable" => "Выбранное окно недоступно. Можно скопировать черновик.",
        "unsupported_desktop" => "Для голосового режима нужен Hyprland или KDE Wayland.",
        "backlog" => "Распознавание не успевает. Режим остановлен; проверьте черновик.",
        "out_of_memory" => "Недостаточно памяти для распознавания. Попробуйте модель поменьше.",
        "unsupported_model" => {
            "Модель не поддерживает распознавание в реальном времени. Проверьте модель и язык."
        }
        "microphone" => "Микрофон отключился. Проверьте подключение и включите режим снова.",
        "timeout" => {
            "Распознавание заняло слишком много времени. Режим остановлен; проверьте черновик."
        }
        "desktop" => {
            "Не удалось вставить текст. Вернитесь в выбранное окно или скопируйте черновик."
        }
        "busy" => "Предыдущая запись ещё завершается. Попробуйте чуть позже.",
        "cancelled" => "Вставка отменена. Можно скопировать черновик.",
        "delivery_uncertain" => {
            "Текст мог быть вставлен или отправлен. Проверьте окно; черновик сохранён."
        }
        "interrupted" => "Диктор остановился до вставки текста. Текст сохранён.",
        _ => "Распознавание прервалось. Выключите режим и попробуйте снова.",
    }
}

/// What the bar shows. Comparing this, not the snapshot, skips countdown ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: &'static str,
    tooltip: &'static str,
    language: String,
    rescue: Vec<Phrase>,
}

fn shown(snapshot: Option<&Snapshot>, rescue: &[Phrase], language: &str) -> Shown {
    let (state, tooltip) = match snapshot.map(|s| (s.status, s.speech_active, s.error.as_deref())) {
        None | Some((Status::Off, ..)) => ("off", ""),
        Some((Status::Stopping, ..)) => ("off", status_text("stopping")),
        Some((Status::Loading, ..)) => ("listening", status_text("loading")),
        Some((Status::Listening, true, _)) => ("speech", status_text("speech")),
        Some((Status::Listening, false, _)) => ("listening", status_text("listening")),
        Some((Status::Committing, ..)) => ("sending", status_text("committing")),
        // A refused paste keeps listening; its text is in the popup.
        Some((Status::Draft, ..)) => ("listening", status_text("listening")),
        Some((Status::Error, _, error)) => ("error", reason_text(error.unwrap_or("model"))),
    };
    Shown {
        state,
        tooltip,
        language: language.to_string(),
        rescue: rescue.to_vec(),
    }
}

#[derive(Serialize)]
struct RescueFile {
    /// Newest phrase id: a new failure reopens a popup the bar had hidden.
    id: String,
    count: usize,
    title: String,
    reason: &'static str,
    /// Every phrase the popup rescues, oldest first; Copy copies these.
    text: String,
    copy: &'static str,
    close: &'static str,
}

#[derive(Serialize)]
struct BarFile<'a> {
    state: &'a str,
    tooltip: &'a str,
    /// Recognition language: "auto", "ru", "en", ...
    language: &'a str,
    rescue: Option<RescueFile>,
}

fn render(shown: &Shown) -> BarFile<'_> {
    let rescue = shown.rescue.last().map(|newest| RescueFile {
        id: newest.id.to_string(),
        count: shown.rescue.len(),
        title: format!("Не вставлено: {}", shown.rescue.len()),
        reason: reason_text(&newest.reason),
        text: joined(&shown.rescue),
        copy: "Скопировать",
        close: "Закрыть",
    });
    BarFile {
        state: shown.state,
        tooltip: shown.tooltip,
        language: &shown.language,
        rescue,
    }
}

pub struct BarState {
    path: PathBuf,
    last: Mutex<Option<Shown>>,
}

impl BarState {
    /// `<runtime dir>/voice-state.json`, the file the widget watches.
    pub fn new(runtime: PathBuf) -> Self {
        Self {
            path: runtime.join("voice-state.json"),
            last: Mutex::new(None),
        }
    }

    /// `snapshot` None is "voice mode off"; written at startup so a crash
    /// cannot leave a stale "listening" behind.
    pub fn show(&self, snapshot: Option<&Snapshot>, rescue: &[Phrase], language: &str) {
        let shown = shown(snapshot, rescue, language);
        let mut last = lock(&self.last);
        if last.as_ref() == Some(&shown) {
            return;
        }
        match self.write(&render(&shown)) {
            Ok(()) => *last = Some(shown),
            Err(error) => log::warn!("Voice bar state not written: {error}"),
        }
    }

    fn write(&self, file: &BarFile) -> std::io::Result<()> {
        let directory = self.path.parent().ok_or(std::io::ErrorKind::NotFound)?;
        std::fs::create_dir_all(directory)?;
        use std::os::unix::fs::PermissionsExt;
        // The popup quotes dictated text: owner-only.
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(file)?)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

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

    fn phrase(id: i64, text: &str, reason: &str) -> Phrase {
        Phrase {
            id,
            created_at: id,
            text: text.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn icon_is_only_the_live_state_even_with_text_to_rescue() {
        let rescue = [phrase(1, "текст", "target_changed")];
        let state =
            |status, speech| shown(Some(&snapshot(status, speech, None)), &rescue, "auto").state;
        assert_eq!(shown(None, &rescue, "auto").state, "off");
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
    fn rescue_popup_carries_every_phrase_and_the_newest_reason() {
        let shown = shown(
            None,
            &[
                phrase(3, "первая ", "target_changed"),
                phrase(5, "вторая", "interrupted"),
            ],
            "en",
        );
        let json: Value = serde_json::to_value(render(&shown)).unwrap();
        assert_eq!(json["state"], "off");
        assert_eq!(json["language"], "en");
        assert_eq!(json["rescue"]["id"], "5");
        assert_eq!(json["rescue"]["count"], 2);
        assert_eq!(json["rescue"]["title"], "Не вставлено: 2");
        assert_eq!(json["rescue"]["text"], "первая вторая");
        assert_eq!(json["rescue"]["reason"], reason_text("interrupted"));
        assert_eq!(json["rescue"]["copy"], "Скопировать");
        assert_eq!(json["rescue"]["close"], "Закрыть");
        let none: Value = serde_json::to_value(render(&super::shown(None, &[], "auto"))).unwrap();
        assert!(none["rescue"].is_null());
        let error = super::shown(
            Some(&snapshot(Status::Error, false, Some("weird"))),
            &[],
            "auto",
        );
        assert_eq!(error.tooltip, reason_text("model"));
    }

    #[test]
    fn writes_only_real_changes_and_owner_only() {
        let directory = std::env::temp_dir().join(format!("diktor-bar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let bar = BarState::new(directory.clone());
        let mut listening = snapshot(Status::Listening, false, None);
        bar.show(Some(&listening), &[], "ru");
        let first = std::fs::metadata(&bar.path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        listening.revision += 1;
        listening.silence_remaining_ms = Some(500);
        bar.show(Some(&listening), &[], "ru");
        assert_eq!(
            std::fs::metadata(&bar.path).unwrap().modified().unwrap(),
            first
        );
        bar.show(None, &[phrase(1, "текст", "cancelled")], "ru");
        let written: Value = serde_json::from_slice(&std::fs::read(&bar.path).unwrap()).unwrap();
        assert_eq!(written["state"], "off");
        assert_eq!(written["rescue"]["text"], "текст");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bar.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
