//! `~/.local/share/diktor/diktor.db`: every recognised phrase, committed to
//! disk before its paste is attempted, then marked delivered or failed with
//! the reason. Failed phrases the owner has not copied are "waiting"; those
//! not dismissed either are the "rescue" the bar popup shows. Rows of a run
//! that ended before an outcome was written (crash, kill -9) become failed
//! with reason `interrupted` when the next daemon or CLI opens the database.
use crate::voice_session::state::{lock, Target};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS phrases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run TEXT NOT NULL,
    session INTEGER NOT NULL,
    phrase INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    text TEXT NOT NULL,
    target_app TEXT,
    target_window TEXT,
    status TEXT NOT NULL CHECK (status IN ('pending', 'delivered', 'failed')),
    reason TEXT,
    settled_at INTEGER,
    copied_at INTEGER,
    dismissed_at INTEGER,
    UNIQUE (run, session, phrase)
);
CREATE INDEX IF NOT EXISTS phrases_waiting ON phrases (status, copied_at, dismissed_at);
PRAGMA user_version = 1;
";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A phrase no window received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phrase {
    pub id: i64,
    pub created_at: i64,
    pub text: String,
    /// Paste refusal or stop code; `interrupted` for a run that died first.
    pub reason: String,
}

pub struct Journal {
    connection: Mutex<Connection>,
    /// This process: rows of other runs still pending were interrupted.
    run: String,
}

impl Journal {
    /// Opens (creating) `directory/diktor.db` and applies retention.
    pub fn open(directory: &Path, keep_days: u32, keep_phrases: u32) -> rusqlite::Result<Self> {
        let run = format!("{}-{}", now_ms(), std::process::id());
        Self::open_at(&directory.join("diktor.db"), run, keep_days, keep_phrases)
    }

    fn open_at(
        path: &Path,
        run: String,
        keep_days: u32,
        keep_phrases: u32,
    ) -> rusqlite::Result<Self> {
        if let Some(directory) = path.parent() {
            let _ = std::fs::create_dir_all(directory);
        }
        let connection = Connection::open(path)?;
        // The CLI and the daemon write concurrently; neither may fail a commit.
        connection.busy_timeout(Duration::from_secs(3))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        // A commit is on disk when it returns.
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(SCHEMA)?;
        let journal = Self {
            connection: Mutex::new(connection),
            run,
        };
        journal.retain(keep_days, keep_phrases)?;
        Ok(journal)
    }

    fn retain(&self, keep_days: u32, keep_phrases: u32) -> rusqlite::Result<()> {
        let oldest = now_ms() - i64::from(keep_days) * 24 * 60 * 60 * 1000;
        lock(&self.connection).execute(
            "DELETE FROM phrases WHERE created_at < ?1
               OR id NOT IN (SELECT id FROM phrases ORDER BY id DESC LIMIT ?2)",
            params![oldest, keep_phrases],
        )?;
        Ok(())
    }

    /// Pending rows of runs that are not this one can never be delivered.
    pub fn interrupt_pending(&self) -> rusqlite::Result<usize> {
        lock(&self.connection).execute(
            "UPDATE phrases SET status = 'failed', reason = 'interrupted', settled_at = ?2
             WHERE status = 'pending' AND run != ?1",
            params![self.run, now_ms()],
        )
    }

    /// On disk when this returns; call before any paste of the phrase.
    pub fn phrase(
        &self,
        session: u64,
        phrase: u64,
        text: &str,
        target: Option<&Target>,
    ) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "INSERT OR IGNORE INTO phrases
                 (run, session, phrase, created_at, text, target_app, target_window, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
            params![
                self.run,
                session,
                phrase,
                now_ms(),
                text,
                target.map(|t| &t.app_id),
                target.map(|t| &t.id)
            ],
        )?;
        Ok(())
    }

    fn settle(
        &self,
        session: u64,
        phrase: u64,
        status: &str,
        reason: Option<&str>,
        target: Option<&Target>,
    ) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "UPDATE phrases SET status = ?4, reason = ?5, settled_at = ?6,
                 target_app = COALESCE(?7, target_app), target_window = COALESCE(?8, target_window)
             WHERE run = ?1 AND session = ?2 AND phrase = ?3",
            params![
                self.run,
                session,
                phrase,
                status,
                reason,
                now_ms(),
                target.map(|t| &t.app_id),
                target.map(|t| &t.id)
            ],
        )?;
        Ok(())
    }

    pub fn delivered(&self, session: u64, phrase: u64, target: &Target) -> rusqlite::Result<()> {
        self.settle(session, phrase, "delivered", None, Some(target))
    }

    pub fn failed(
        &self,
        session: u64,
        phrase: u64,
        reason: &str,
        target: Option<&Target>,
    ) -> rusqlite::Result<()> {
        self.settle(session, phrase, "failed", Some(reason), target)
    }

    fn select(&self, filter: &str, limit: i64) -> rusqlite::Result<Vec<Phrase>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(&format!(
            "SELECT id, created_at, text, COALESCE(reason, status) FROM phrases
             WHERE {filter} ORDER BY id DESC LIMIT ?1"
        ))?;
        let mut rows = statement
            .query_map(params![limit], |row| {
                Ok(Phrase {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    text: row.get(2)?,
                    reason: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Failed and not copied yet, oldest first.
    pub fn waiting(&self) -> rusqlite::Result<Vec<Phrase>> {
        self.select("status = 'failed' AND copied_at IS NULL", i64::MAX)
    }

    /// What the bar popup offers: waiting and not dismissed, oldest first.
    pub fn rescue(&self) -> rusqlite::Result<Vec<Phrase>> {
        self.select(
            "status = 'failed' AND copied_at IS NULL AND dismissed_at IS NULL",
            i64::MAX,
        )
    }

    /// The newest `n` phrases no window received, copied or not, oldest first.
    pub fn last_undelivered(&self, n: usize) -> rusqlite::Result<Vec<Phrase>> {
        self.select("status = 'failed'", i64::try_from(n).unwrap_or(i64::MAX))
    }

    fn mark(&self, column: &str, ids: &[i64]) -> rusqlite::Result<()> {
        let mut connection = lock(&self.connection);
        let transaction = connection.transaction()?;
        let at = now_ms();
        for id in ids {
            transaction.execute(
                &format!("UPDATE phrases SET {column} = ?2 WHERE id = ?1"),
                params![id, at],
            )?;
        }
        transaction.commit()
    }

    pub fn copied(&self, ids: &[i64]) -> rusqlite::Result<()> {
        self.mark("copied_at", ids)
    }

    /// Hides phrases from the popup; they stay waiting for `diktor copy`.
    pub fn dismissed(&self, ids: &[i64]) -> rusqlite::Result<()> {
        self.mark("dismissed_at", ids)
    }

    /// `(status, reason)` of one phrase of this run.
    #[cfg(test)]
    pub fn outcome(&self, session: u64, phrase: u64) -> Option<(String, Option<String>)> {
        lock(&self.connection)
            .query_row(
                "SELECT status, reason FROM phrases WHERE run = ?1 AND session = ?2 AND phrase = ?3",
                params![self.run, session, phrase],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
    }
}

/// Clipboard and primary selection, like the bridge's copy; direct, so a
/// rescue works while the helper (or the daemon) is dead.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    for primary in [false, true] {
        let mut command = std::process::Command::new("wl-copy");
        if primary {
            command.arg("--primary");
        }
        let mut child = command
            .args(["--type", "text/plain;charset=utf-8"])
            .stdin(std::process::Stdio::piped())
            // wl-copy stays behind serving the selection; it must not hold
            // the caller's stdout (a `$(diktor copy)` would never finish).
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("wl-copy: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("wl-copy: no stdin")?
            .write_all(text.as_bytes())
            .map_err(|e| format!("wl-copy: {e}"))?;
        let status = child.wait().map_err(|e| format!("wl-copy: {e}"))?;
        if !status.success() {
            return Err(format!("wl-copy: {status}"));
        }
    }
    Ok(())
}

/// Phrases joined for the clipboard. Spaces, not line breaks: a newline
/// pasted into a chat would send it.
pub fn joined(phrases: &[Phrase]) -> String {
    phrases
        .iter()
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn database(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("diktor-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("diktor.db")
    }

    fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            app_id: "foot".into(),
            pid: 7,
            output: "DP-1".into(),
            output_x: 0,
            output_y: 0,
        }
    }

    fn texts(phrases: &[Phrase]) -> Vec<(&str, &str)> {
        phrases
            .iter()
            .map(|p| (p.text.as_str(), p.reason.as_str()))
            .collect()
    }

    #[test]
    fn pending_before_paste_then_delivered_never_waits() {
        let path = database("delivered");
        let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
        journal.phrase(1, 1, "вставлено", None).unwrap();
        assert_eq!(journal.outcome(1, 1), Some(("pending".into(), None)));
        journal.delivered(1, 1, &target("0xA")).unwrap();
        assert_eq!(journal.outcome(1, 1), Some(("delivered".into(), None)));
        assert!(journal.waiting().unwrap().is_empty());
        assert!(journal.last_undelivered(5).unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_and_interrupted_wait_until_copied_and_dismiss_only_hides() {
        let path = database("rescue");
        {
            let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
            journal.phrase(1, 1, "не туда", None).unwrap();
            journal
                .failed(1, 1, "target_changed", Some(&target("0xA")))
                .unwrap();
            // Killed between the journal and the paste.
            journal.phrase(1, 2, "оборвалось", None).unwrap();
        }
        let journal = Journal::open_at(&path, "run-b".into(), 30, 1000).unwrap();
        // Still this run's own pending phrase until interrupt_pending.
        assert_eq!(
            texts(&journal.waiting().unwrap()),
            [("не туда", "target_changed")]
        );
        assert_eq!(journal.interrupt_pending().unwrap(), 1);
        let rescue = journal.rescue().unwrap();
        assert_eq!(
            texts(&rescue),
            [("не туда", "target_changed"), ("оборвалось", "interrupted")]
        );
        journal.dismissed(&[rescue[0].id]).unwrap();
        assert_eq!(
            texts(&journal.rescue().unwrap()),
            [("оборвалось", "interrupted")]
        );
        assert_eq!(journal.waiting().unwrap().len(), 2);
        journal.copied(&[rescue[1].id]).unwrap();
        assert_eq!(
            texts(&journal.waiting().unwrap()),
            [("не туда", "target_changed")]
        );
        // `copy N` still finds copied phrases, newest N, oldest first.
        assert_eq!(
            texts(&journal.last_undelivered(1).unwrap()),
            [("оборвалось", "interrupted")]
        );
        assert_eq!(journal.last_undelivered(9).unwrap().len(), 2);
        assert_eq!(
            joined(&journal.last_undelivered(9).unwrap()),
            "не туда оборвалось"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn retention_keeps_the_newest_rows() {
        let path = database("retention");
        {
            let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
            for phrase in 1..=5 {
                journal
                    .phrase(1, phrase, &format!("фраза {phrase}"), None)
                    .unwrap();
                journal.failed(1, phrase, "cancelled", None).unwrap();
            }
        }
        let journal = Journal::open_at(&path, "run-b".into(), 30, 2).unwrap();
        assert_eq!(
            texts(&journal.waiting().unwrap()),
            [("фраза 4", "cancelled"), ("фраза 5", "cancelled")]
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
