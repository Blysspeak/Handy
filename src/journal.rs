//! `~/.local/share/diktor/diktor.db`: every recognised phrase, committed to
//! disk before its paste is attempted, then marked delivered or failed with
//! the reason. Failed phrases the owner has not copied are "waiting"; those
//! not dismissed either are the "rescue" the bar popup shows. Rows of a run
//! that ended before an outcome was written (crash, kill -9) become failed
//! with reason `interrupted` when the next daemon or CLI opens the database.
//!
//! The same database holds the glossary (spoken form -> written form, see
//! `glossary.rs`) and the manual vocabulary; every glossary change bumps
//! `meta.glossary_version` through triggers, so the daemon re-reads it before
//! the next phrase whoever wrote it (CLI, sqlite3 by hand).
use crate::glossary::Entries;
use crate::voice_session::state::{lock, Target};
use rusqlite::{params, Connection};
use serde::Serialize;
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
    -- What the model recognised, when the glossary changed it into `text`.
    raw_text TEXT,
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
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO meta (key, value) VALUES ('glossary_version', 0);
INSERT OR IGNORE INTO meta (key, value) VALUES ('vocabulary_version', 0);
CREATE TABLE IF NOT EXISTS glossary (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_text TEXT NOT NULL,
    to_text TEXT NOT NULL,
    -- 'word': that exact word; 'stem': also Russian case endings and a glued preposition.
    kind TEXT NOT NULL DEFAULT 'word' CHECK (kind IN ('word', 'stem')),
    source TEXT NOT NULL DEFAULT 'owner' CHECK (source IN ('owner', 'claude')),
    note TEXT,
    created_at INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    UNIQUE (from_text, kind)
);
CREATE TRIGGER IF NOT EXISTS glossary_insert AFTER INSERT ON glossary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'glossary_version'; END;
CREATE TRIGGER IF NOT EXISTS glossary_update AFTER UPDATE ON glossary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'glossary_version'; END;
CREATE TRIGGER IF NOT EXISTS glossary_delete AFTER DELETE ON glossary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'glossary_version'; END;
CREATE TABLE IF NOT EXISTS vocabulary (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    term TEXT NOT NULL UNIQUE COLLATE NOCASE,
    source TEXT NOT NULL DEFAULT 'owner' CHECK (source IN ('owner', 'claude')),
    note TEXT,
    created_at INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TRIGGER IF NOT EXISTS vocabulary_insert AFTER INSERT ON vocabulary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'vocabulary_version'; END;
CREATE TRIGGER IF NOT EXISTS vocabulary_update AFTER UPDATE ON vocabulary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'vocabulary_version'; END;
CREATE TRIGGER IF NOT EXISTS vocabulary_delete AFTER DELETE ON vocabulary
    BEGIN UPDATE meta SET value = value + 1 WHERE key = 'vocabulary_version'; END;
PRAGMA user_version = 3;
";

/// Imported once, on the first open of a database (`meta.seeded`); entries
/// the owner removes later stay removed.
const GLOSSARY_SEED: &str = include_str!("../contrib/glossary.seed.toml");
const VOCABULARY_SEED: &[&str] = &[
    "Hyprland",
    "Omarchy",
    "CachyOS",
    "Quickshell",
    "Claude",
    "Claude Code",
    "Opus",
    "Sonnet",
    "Fable",
    "Haiku",
    "Nemotron",
    "Voxtral",
    "Kokoro",
    "X-Hub",
    "xhub",
    "aurelius",
    "ulika",
    "aletix",
    "boostix",
    "grelay",
    "Telegram",
    "Bitwarden",
    "rclone",
    "GitHub",
    "commit",
    "push",
    "pull request",
    "merge",
    "branch",
    "worktree",
    "deploy",
    "rebase",
];

/// One glossary row, for `diktor glossary list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GlossaryRow {
    pub id: i64,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub source: String,
    pub note: Option<String>,
    pub created_at: i64,
    pub enabled: bool,
}

/// One manual vocabulary term, for `diktor vocab list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VocabularyRow {
    pub term: String,
    pub source: String,
    pub note: Option<String>,
    pub created_at: i64,
}

/// One phrase, for `diktor phrases recent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhraseRow {
    pub id: i64,
    pub created_at: i64,
    /// What the model recognised.
    pub raw: String,
    /// What was pasted (after the glossary).
    pub text: String,
    pub status: String,
    pub reason: Option<String>,
    pub target_app: Option<String>,
    /// The owner's (or Claude's) correction: a reference sample for benchmarks.
    pub corrected: Option<String>,
}

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
        let has_raw: bool = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('phrases') WHERE name = 'raw_text'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_raw {
            connection.execute_batch("ALTER TABLE phrases ADD COLUMN raw_text TEXT")?;
        }
        let has_correction: bool = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('phrases') WHERE name = 'corrected_text'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_correction {
            connection.execute_batch(
                "ALTER TABLE phrases ADD COLUMN corrected_text TEXT;
                 ALTER TABLE phrases ADD COLUMN corrected_at INTEGER;",
            )?;
        }
        let journal = Self {
            connection: Mutex::new(connection),
            run,
        };
        journal.retain(keep_days, keep_phrases)?;
        journal.seed()?;
        Ok(journal)
    }

    fn seed(&self) -> rusqlite::Result<()> {
        let mut connection = lock(&self.connection);
        let transaction = connection.transaction()?;
        let seeded = transaction
            .query_row("SELECT value FROM meta WHERE key = 'seeded'", [], |row| {
                row.get::<_, i64>(0)
            })
            .is_ok();
        if !seeded {
            let entries: Entries = toml::from_str(GLOSSARY_SEED)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let now = now_ms();
            for (kind, map) in [("word", &entries.words), ("stem", &entries.stems)] {
                for (from, to) in map {
                    transaction.execute(
                        "INSERT OR IGNORE INTO glossary (from_text, to_text, kind, note, created_at)
                         VALUES (?1, ?2, ?3, 'seed', ?4)",
                        params![crate::glossary::normalise(from), to, kind, now],
                    )?;
                }
            }
            for term in VOCABULARY_SEED {
                transaction.execute(
                    "INSERT OR IGNORE INTO vocabulary (term, note, created_at) VALUES (?1, 'seed', ?2)",
                    params![term, now],
                )?;
            }
            transaction.execute("INSERT INTO meta (key, value) VALUES ('seeded', 1)", [])?;
        }
        transaction.commit()
    }

    /// Bumped by triggers on every glossary change.
    pub fn glossary_version(&self) -> rusqlite::Result<i64> {
        lock(&self.connection).query_row(
            "SELECT value FROM meta WHERE key = 'glossary_version'",
            [],
            |row| row.get(0),
        )
    }

    /// Bumped by triggers on every vocabulary change.
    pub fn vocabulary_version(&self) -> rusqlite::Result<i64> {
        lock(&self.connection).query_row(
            "SELECT value FROM meta WHERE key = 'vocabulary_version'",
            [],
            |row| row.get(0),
        )
    }

    pub fn glossary_rows(&self) -> rusqlite::Result<Vec<GlossaryRow>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(
            "SELECT id, from_text, to_text, kind, source, note, created_at, enabled
             FROM glossary ORDER BY kind DESC, from_text",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(GlossaryRow {
                id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
                kind: row.get(3)?,
                source: row.get(4)?,
                note: row.get(5)?,
                created_at: row.get(6)?,
                enabled: row.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect()
    }

    /// Enabled entries, as the matcher takes them.
    pub fn glossary(&self) -> rusqlite::Result<Entries> {
        let mut entries = Entries::default();
        for row in self.glossary_rows()?.into_iter().filter(|row| row.enabled) {
            let map = if row.kind == "stem" {
                &mut entries.stems
            } else {
                &mut entries.words
            };
            map.insert(row.from, row.to);
        }
        Ok(entries)
    }

    /// Adds or replaces the entry for `from` of this kind.
    /// `enabled` false keeps the entry pending (Claude's suggestion).
    #[allow(clippy::too_many_arguments)]
    pub fn glossary_add(
        &self,
        from: &str,
        to: &str,
        stem: bool,
        source: &str,
        note: Option<&str>,
        enabled: bool,
    ) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "INSERT INTO glossary (from_text, to_text, kind, source, note, created_at, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (from_text, kind) DO UPDATE SET
                 to_text = excluded.to_text, source = excluded.source,
                 note = excluded.note, created_at = excluded.created_at,
                 enabled = excluded.enabled",
            params![
                crate::glossary::normalise(from),
                to,
                if stem { "stem" } else { "word" },
                source,
                note,
                now_ms(),
                i64::from(enabled)
            ],
        )?;
        Ok(())
    }

    /// Turns pending entries for `from` on; returns how many.
    pub fn glossary_approve(&self, from: &str) -> rusqlite::Result<usize> {
        lock(&self.connection).execute(
            "UPDATE glossary SET enabled = 1 WHERE from_text = ?1 AND enabled = 0",
            params![crate::glossary::normalise(from)],
        )
    }

    /// Phrases whose recognised, pasted or corrected text contains `query`,
    /// case-insensitively for Cyrillic too (SQLite's lower() is ASCII only;
    /// retention keeps the table small enough to filter here).
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<PhraseRow>> {
        let query = query.to_lowercase();
        let found = self
            .recent(usize::MAX)?
            .into_iter()
            .filter(|row| {
                [Some(&row.text), Some(&row.raw), row.corrected.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|text| text.to_lowercase().contains(&query))
            })
            .take(limit)
            .collect();
        Ok(found)
    }

    /// The text of phrase `id` to copy: the correction when there is one.
    pub fn phrase_text(&self, id: i64) -> rusqlite::Result<Option<String>> {
        match lock(&self.connection).query_row(
            "SELECT COALESCE(corrected_text, text) FROM phrases WHERE id = ?1",
            params![id],
            |row| row.get(0),
        ) {
            Ok(text) => Ok(Some(text)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Removes `from` (both kinds unless one is given); returns rows removed.
    pub fn glossary_remove(&self, from: &str, kind: Option<&str>) -> rusqlite::Result<usize> {
        lock(&self.connection).execute(
            "DELETE FROM glossary WHERE from_text = ?1 AND (?2 IS NULL OR kind = ?2)",
            params![crate::glossary::normalise(from), kind],
        )
    }

    pub fn vocabulary(&self) -> rusqlite::Result<Vec<VocabularyRow>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(
            "SELECT term, source, note, created_at FROM vocabulary WHERE enabled = 1 ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(VocabularyRow {
                term: row.get(0)?,
                source: row.get(1)?,
                note: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn vocabulary_add(
        &self,
        term: &str,
        source: &str,
        note: Option<&str>,
    ) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "INSERT INTO vocabulary (term, source, note, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (term) DO UPDATE SET enabled = 1, source = excluded.source, note = excluded.note",
            params![term.trim(), source, note, now_ms()],
        )?;
        Ok(())
    }

    pub fn vocabulary_remove(&self, term: &str) -> rusqlite::Result<usize> {
        lock(&self.connection).execute(
            "DELETE FROM vocabulary WHERE term = ?1",
            params![term.trim()],
        )
    }

    /// Newest `limit` phrases, newest first.
    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<PhraseRow>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(
            "SELECT id, created_at, COALESCE(raw_text, text), text, status, reason, target_app,
                    corrected_text
             FROM phrases ORDER BY id DESC LIMIT ?1",
        )?;
        let rows =
            statement.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(PhraseRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    raw: row.get(2)?,
                    text: row.get(3)?,
                    status: row.get(4)?,
                    reason: row.get(5)?,
                    target_app: row.get(6)?,
                    corrected: row.get(7)?,
                })
            })?;
        rows.collect()
    }

    /// Stores the right text for phrase `id`; false when there is no such phrase.
    pub fn correct(&self, id: i64, text: &str) -> rusqlite::Result<bool> {
        Ok(lock(&self.connection).execute(
            "UPDATE phrases SET corrected_text = ?2, corrected_at = ?3 WHERE id = ?1",
            params![id, text, now_ms()],
        )? == 1)
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
        raw: Option<&str>,
        target: Option<&Target>,
    ) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "INSERT OR IGNORE INTO phrases
                 (run, session, phrase, created_at, text, raw_text, target_app, target_window, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
            params![
                self.run,
                session,
                phrase,
                now_ms(),
                text,
                raw,
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

    /// What the model recognised (before the glossary) since `since_ms`.
    pub fn recognised_since(&self, since_ms: i64) -> rusqlite::Result<Vec<String>> {
        let connection = lock(&self.connection);
        let mut statement = connection.prepare(
            "SELECT COALESCE(raw_text, text) FROM phrases WHERE created_at >= ?1 ORDER BY id",
        )?;
        let rows = statement.query_map(params![since_ms], |row| row.get(0))?;
        rows.collect()
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
        journal.phrase(1, 1, "вставлено", None, None).unwrap();
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
            journal.phrase(1, 1, "не туда", None, None).unwrap();
            journal
                .failed(1, 1, "target_changed", Some(&target("0xA")))
                .unwrap();
            // Killed between the journal and the paste.
            journal.phrase(1, 2, "оборвалось", None, None).unwrap();
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
    fn raw_text_is_kept_next_to_the_glossary_text() {
        let path = database("raw");
        let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
        journal
            .phrase(1, 1, "сделай commit", Some("сделай коммит"), None)
            .unwrap();
        let raw: Option<String> = lock(&journal.connection)
            .query_row("SELECT raw_text FROM phrases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw.as_deref(), Some("сделай коммит"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn glossary_lives_in_the_database_seeded_once_and_versioned() {
        let path = database("glossary");
        let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
        let seeded = journal.glossary().unwrap();
        assert_eq!(seeded.stems.get("иксхаб").map(String::as_str), Some("xhub"));
        assert_eq!(
            seeded.words.get("коммит").map(String::as_str),
            Some("commit")
        );
        assert!(journal
            .vocabulary()
            .unwrap()
            .iter()
            .any(|v| v.term == "Hyprland"));
        let version = journal.glossary_version().unwrap();
        journal
            .glossary_add(
                "Пул  Реквест",
                "pull request",
                false,
                "claude",
                Some("from chat"),
                true,
            )
            .unwrap();
        assert!(journal.glossary_version().unwrap() > version);
        let row = journal
            .glossary_rows()
            .unwrap()
            .into_iter()
            .find(|row| row.from == "пул реквест")
            .unwrap();
        assert_eq!(
            (row.source.as_str(), row.note.as_deref()),
            ("claude", Some("from chat"))
        );
        // Removing a seed entry sticks: seeding runs once per database.
        assert_eq!(journal.glossary_remove("коммит", None).unwrap(), 1);
        drop(journal);
        let journal = Journal::open_at(&path, "run-b".into(), 30, 1000).unwrap();
        assert!(!journal.glossary().unwrap().words.contains_key("коммит"));
        assert!(journal
            .glossary()
            .unwrap()
            .words
            .contains_key("пул реквест"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn recent_phrases_carry_raw_final_and_correction() {
        let path = database("recent");
        let journal = Journal::open_at(&path, "run-a".into(), 30, 1000).unwrap();
        journal
            .phrase(1, 1, "сделай commit", Some("сделай камит"), None)
            .unwrap();
        journal.phrase(1, 2, "вторая", None, None).unwrap();
        let recent = journal.recent(10).unwrap();
        assert_eq!(recent[0].text, "вторая");
        assert_eq!(recent[1].raw, "сделай камит");
        assert_eq!(recent[1].text, "сделай commit");
        assert!(journal
            .correct(recent[1].id, "сделай commit и push")
            .unwrap());
        assert!(!journal.correct(9999, "нет такой").unwrap());
        assert_eq!(journal.search("КАМИТ", 10).unwrap().len(), 1);
        assert_eq!(journal.search("push", 10).unwrap()[0].id, recent[1].id);
        assert_eq!(
            journal.phrase_text(recent[1].id).unwrap().as_deref(),
            Some("сделай commit и push")
        );
        assert_eq!(journal.phrase_text(9999).unwrap(), None);
        assert_eq!(
            journal.recent(10).unwrap()[1].corrected.as_deref(),
            Some("сделай commit и push")
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
                    .phrase(1, phrase, &format!("фраза {phrase}"), None, None)
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
