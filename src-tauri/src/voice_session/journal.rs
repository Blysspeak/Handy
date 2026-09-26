//! Durable record of every dictated phrase, so text survives a failed paste,
//! a crash or a restart. A phrase is committed to the `voice_phrases` table of
//! history.db before its paste is attempted, then marked delivered or failed
//! (with the reason); the owner's copy of rescued text sets `copied_at`.
use super::state::{lock, Target};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retention, applied when the journal is opened: 30 days, newest 1000 phrases.
const KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const KEEP_PHRASES: i64 = 1000;

/// Created on every open, deliberately OUTSIDE the rusqlite_migration list in
/// managers/history.rs: a migration would raise PRAGMA user_version past 4,
/// and older builds sharing this data directory (the CachyOS install) then
/// fail with DatabaseTooFarAhead and panic in HistoryManager::new
/// (lib.rs: `.expect("Failed to initialize history manager")`). A table they
/// do not know is invisible to them. Any future change to this table must stay
/// backward compatible the same way: IF NOT EXISTS / additive, never versioned.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS voice_phrases (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    text TEXT NOT NULL,
    target_app TEXT,
    target_window TEXT,
    status TEXT NOT NULL CHECK (status IN ('pending', 'delivered', 'failed')),
    reason TEXT,
    delivered_at INTEGER,
    copied_at INTEGER
);
CREATE INDEX IF NOT EXISTS voice_phrases_rescue
    ON voice_phrases (copied_at, status, created_at);
";

/// Text that no window has received and the owner has not copied yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub id: String,
    pub at: i64,
    pub text: String,
    /// Paste refusal code, or "interrupted" for a phrase whose run ended
    /// before any outcome was written (crash, kill -9).
    pub reason: String,
}

pub struct Journal {
    connection: Mutex<Connection>,
    /// Prefix of this process's ids; pending phrases of other runs were interrupted.
    run: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Journal {
    /// Opens `directory/history.db`, creating the table and applying retention.
    pub fn open(directory: &Path) -> rusqlite::Result<Self> {
        Self::open_at(&directory.join("history.db"), now_ms())
    }

    fn open_at(path: &Path, now: i64) -> rusqlite::Result<Self> {
        if let Some(directory) = path.parent() {
            let _ = std::fs::create_dir_all(directory);
        }
        let connection = Connection::open(path)?;
        // History writes from the other connection must not fail our commit.
        connection.busy_timeout(Duration::from_secs(2))?;
        // A commit is on disk when it returns (rollback journal, file mode unchanged).
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(SCHEMA)?;
        connection.execute(
            "DELETE FROM voice_phrases WHERE created_at < ?1
               OR id NOT IN (SELECT id FROM voice_phrases ORDER BY created_at DESC LIMIT ?2)",
            params![now - KEEP_MS, KEEP_PHRASES],
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            run: now.to_string(),
        })
    }

    /// Stable id of one phrase of one voice session in this process.
    pub fn id(&self, generation: u64, phrase: u64) -> String {
        format!("{}-{generation}-{phrase}", self.run)
    }

    /// Committed before this returns; call before any paste of the phrase.
    pub fn phrase(&self, id: &str, text: &str, target: Option<&Target>) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "INSERT OR IGNORE INTO voice_phrases
                 (id, created_at, text, target_app, target_window, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
            params![
                id,
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
        id: &str,
        status: &str,
        reason: Option<&str>,
        target: Option<&Target>,
    ) -> rusqlite::Result<()> {
        let delivered_at = (status == "delivered").then(now_ms);
        lock(&self.connection).execute(
            "UPDATE voice_phrases SET status = ?2, reason = ?3, delivered_at = ?4,
                 target_app = COALESCE(target_app, ?5), target_window = COALESCE(target_window, ?6)
             WHERE id = ?1",
            params![
                id,
                status,
                reason,
                delivered_at,
                target.map(|t| &t.app_id),
                target.map(|t| &t.id)
            ],
        )?;
        Ok(())
    }

    pub fn delivered(&self, id: &str, target: Option<&Target>) -> rusqlite::Result<()> {
        self.settle(id, "delivered", None, target)
    }

    pub fn failed(&self, id: &str, reason: &str, target: Option<&Target>) -> rusqlite::Result<()> {
        self.settle(id, "failed", Some(reason), target)
    }

    pub fn copied(&self, ids: &[String]) -> rusqlite::Result<()> {
        let mut connection = lock(&self.connection);
        let transaction = connection.transaction()?;
        let at = now_ms();
        for id in ids {
            transaction.execute(
                "UPDATE voice_phrases SET copied_at = ?2 WHERE id = ?1",
                params![id, at],
            )?;
        }
        transaction.commit()
    }

    /// Oldest first: failed phrases, and pending phrases of earlier runs that
    /// never got an outcome. This run's pending phrases are still being pasted.
    pub fn waiting(&self) -> rusqlite::Result<Vec<Waiting>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare_cached(
            "SELECT id, created_at, text, status, reason FROM voice_phrases
             WHERE copied_at IS NULL
               AND (status = 'failed' OR (status = 'pending' AND id NOT LIKE ?1))
             ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map(params![format!("{}-%", self.run)], |row| {
            let status: String = row.get(3)?;
            let reason: Option<String> = row.get(4)?;
            Ok(Waiting {
                id: row.get(0)?,
                at: row.get(1)?,
                text: row.get(2)?,
                reason: if status == "pending" {
                    "interrupted".into()
                } else {
                    reason.unwrap_or_else(|| "model".into())
                },
            })
        })?;
        rows.collect()
    }

    #[cfg(test)]
    fn status(&self, id: &str) -> Option<(String, Option<String>, Option<String>)> {
        lock(&self.connection)
            .query_row(
                "SELECT status, reason, target_app FROM voice_phrases WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .ok()
            .flatten()
    }
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
        directory.join("history.db")
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

    fn texts(journal: &Journal) -> Vec<(String, String)> {
        journal
            .waiting()
            .unwrap()
            .into_iter()
            .map(|w| (w.text, w.reason))
            .collect()
    }

    #[test]
    fn voice_journal_leaves_history_schema_and_user_version_alone() {
        let path = database("schema");
        {
            // What a 13.09 build leaves behind: four migrations applied.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE transcription_history (id INTEGER PRIMARY KEY, title TEXT);
                     INSERT INTO transcription_history (title) VALUES ('старая запись');
                     PRAGMA user_version = 4;",
                )
                .unwrap();
        }
        Journal::open_at(&path, 1_000).unwrap();
        Journal::open_at(&path, 2_000).unwrap();
        let connection = Connection::open(&path).unwrap();
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
        let title: String = connection
            .query_row("SELECT title FROM transcription_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "старая запись");
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn voice_journal_phrase_is_committed_before_any_outcome() {
        let path = database("write");
        let journal = Journal::open_at(&path, 1_000).unwrap();
        let id = journal.id(1, 1);
        journal
            .phrase(&id, "первая фраза", Some(&target("0xA")))
            .unwrap();
        // Visible to a second connection: committed, not just buffered.
        let text: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT text FROM voice_phrases WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(text, "первая фраза");
        // In flight in this run: not offered for rescue yet.
        assert!(journal.waiting().unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn voice_journal_marks_delivered_failed_and_copied() {
        let path = database("mark");
        let journal = Journal::open_at(&path, 1_000).unwrap();
        let (a, b, c) = (journal.id(1, 1), journal.id(1, 2), journal.id(1, 3));
        journal.phrase(&a, "доставлена", None).unwrap();
        journal.delivered(&a, Some(&target("0xA"))).unwrap();
        journal.phrase(&b, "не вставилась", None).unwrap();
        journal
            .failed(&b, "target_changed", Some(&target("0xB")))
            .unwrap();
        journal.phrase(&c, "вторая неудача", None).unwrap();
        journal.failed(&c, "cancelled", None).unwrap();
        assert_eq!(
            journal.status(&a),
            Some(("delivered".into(), None, Some("foot".into())))
        );
        assert_eq!(
            texts(&journal),
            [
                ("не вставилась".into(), "target_changed".into()),
                ("вторая неудача".into(), "cancelled".into())
            ]
        );
        journal.copied(&[b.clone(), c.clone()]).unwrap();
        assert!(journal.waiting().unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn voice_journal_delivered_phrase_never_waits_even_after_restart() {
        let path = database("delivered");
        {
            let journal = Journal::open_at(&path, 1_000).unwrap();
            for phrase in 1..=3 {
                let id = journal.id(5, phrase);
                journal.phrase(&id, "вставлено", None).unwrap();
                journal.delivered(&id, Some(&target("0xA"))).unwrap();
            }
            assert!(journal.waiting().unwrap().is_empty());
        }
        let journal = Journal::open_at(&path, 2_000).unwrap();
        assert!(journal.waiting().unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn voice_journal_recovers_failed_and_interrupted_after_restart() {
        let path = database("recover");
        {
            let journal = Journal::open_at(&path, 1_000).unwrap();
            let (a, b) = (journal.id(4, 1), journal.id(4, 2));
            journal.phrase(&a, "упала вставка", None).unwrap();
            journal.failed(&a, "target_changed", None).unwrap();
            // Killed between the committed write and the paste.
            journal.phrase(&b, "процесс убит", None).unwrap();
        }
        let journal = Journal::open_at(&path, 2_000).unwrap();
        assert_eq!(
            texts(&journal),
            [
                ("упала вставка".into(), "target_changed".into()),
                ("процесс убит".into(), "interrupted".into())
            ]
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn voice_journal_retention_drops_old_and_excess_phrases() {
        let path = database("retention");
        {
            let journal = Journal::open_at(&path, 1_000).unwrap();
            let connection = lock(&journal.connection);
            connection
                .execute(
                    "INSERT INTO voice_phrases (id, created_at, text, status, reason)
                     VALUES ('old', 1000, 'старое', 'failed', 'target_changed')",
                    [],
                )
                .unwrap();
            for n in 0..KEEP_PHRASES + 2 {
                connection
                    .execute(
                        "INSERT INTO voice_phrases (id, created_at, text, status)
                         VALUES (?1, ?2, 'фраза', 'delivered')",
                        params![format!("new-{n}"), KEEP_MS + 5_000 + n],
                    )
                    .unwrap();
            }
        }
        let journal = Journal::open_at(&path, KEEP_MS + 10_000).unwrap();
        assert!(journal.waiting().unwrap().is_empty());
        let connection = lock(&journal.connection);
        let (count, oldest): (i64, String) = connection
            .query_row(
                "SELECT COUNT(*), MIN(id) FROM voice_phrases WHERE id LIKE 'new-%'
                   AND created_at = (SELECT MIN(created_at) FROM voice_phrases)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(oldest, "new-2");
        let total: i64 = connection
            .query_row("SELECT COUNT(*) FROM voice_phrases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, KEEP_PHRASES);
        drop(connection);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
