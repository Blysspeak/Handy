//! Durable record of every dictated phrase, so text survives a failed paste,
//! a crash or a restart. Append-only JSON lines, fsync'd per write: a phrase
//! is on disk before its paste is attempted, then marked delivered, failed
//! (with the reason) or copied by the owner. Separate from history.db, whose
//! schema is shared with older builds in the same data directory.
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const FILE_NAME: &str = "voice-journal.jsonl";
/// Retention, applied when the journal is opened.
const KEEP_MS: u64 = 14 * 24 * 60 * 60 * 1000;
const KEEP_PHRASES: usize = 500;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Record {
    Phrase { id: String, at: u64, text: String },
    Delivered { id: String, at: u64 },
    Failed { id: String, at: u64, reason: String },
    Copied { id: String, at: u64 },
}

impl Record {
    fn id(&self) -> &str {
        match self {
            Self::Phrase { id, .. }
            | Self::Delivered { id, .. }
            | Self::Failed { id, .. }
            | Self::Copied { id, .. } => id,
        }
    }
}

/// Text that no window has received and the owner has not copied yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub id: String,
    pub at: u64,
    pub text: String,
    /// Paste refusal code, or "interrupted" for a phrase whose run ended
    /// before any outcome was written (crash, kill, shutdown mid-paste).
    pub reason: String,
}

struct Entry {
    waiting: Waiting,
    /// A delivered/failed/copied record exists.
    settled: bool,
    done: bool,
}

pub struct Journal {
    path: PathBuf,
    /// Prefix of this process's ids; unsettled phrases of other runs were interrupted.
    run: String,
    entries: Mutex<Vec<Entry>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn read(path: &Path) -> std::io::Result<Vec<Record>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    // A crash can leave a torn last line; every complete line still counts.
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

/// Phrases of the last KEEP_MS, at most KEEP_PHRASES, with their outcomes.
fn retained(records: Vec<Record>, now: u64) -> Vec<Record> {
    let phrases: Vec<&str> = records
        .iter()
        .filter_map(|record| match record {
            Record::Phrase { id, at, .. } if now.saturating_sub(*at) <= KEEP_MS => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    let keep: std::collections::HashSet<String> = phrases
        .iter()
        .rev()
        .take(KEEP_PHRASES)
        .map(|id| id.to_string())
        .collect();
    records
        .into_iter()
        .filter(|record| keep.contains(record.id()))
        .collect()
}

impl Journal {
    /// Opens `directory/voice-journal.jsonl`, applying retention.
    pub fn open(directory: &Path) -> std::io::Result<Self> {
        Self::open_at(directory, now_ms())
    }

    fn open_at(directory: &Path, now: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join(FILE_NAME);
        let all = read(&path)?;
        let total = all.len();
        let kept = retained(all, now);
        if kept.len() != total {
            Self::rewrite(&path, &kept)?;
        }
        let journal = Self {
            path,
            run: format!("{now}"),
            entries: Mutex::new(Vec::new()),
        };
        {
            let mut entries = super::state::lock(&journal.entries);
            for record in &kept {
                Self::index(&mut entries, record);
            }
        }
        Ok(journal)
    }

    fn rewrite(path: &Path, records: &[Record]) -> std::io::Result<()> {
        let temporary = path.with_extension("jsonl.tmp");
        let mut file = File::create(&temporary)?;
        for record in records {
            writeln!(file, "{}", serde_json::to_string(record)?)?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(directory) = path.parent() {
            File::open(directory)?.sync_all()?;
        }
        Ok(())
    }

    fn index(entries: &mut Vec<Entry>, record: &Record) {
        match record {
            Record::Phrase { id, at, text } => entries.push(Entry {
                waiting: Waiting {
                    id: id.clone(),
                    at: *at,
                    text: text.clone(),
                    reason: "interrupted".into(),
                },
                settled: false,
                done: false,
            }),
            Record::Delivered { id, .. } | Record::Copied { id, .. } => {
                for entry in entries.iter_mut().filter(|e| &e.waiting.id == id) {
                    entry.settled = true;
                    entry.done = true;
                }
            }
            Record::Failed { id, reason, .. } => {
                for entry in entries.iter_mut().filter(|e| &e.waiting.id == id) {
                    entry.settled = true;
                    entry.waiting.reason = reason.clone();
                }
            }
        }
    }

    fn append(&self, records: &[Record]) -> std::io::Result<()> {
        let mut lines = String::new();
        for record in records {
            lines.push_str(&serde_json::to_string(record)?);
            lines.push('\n');
        }
        // One writer at a time: the file and the index change together.
        let mut entries = super::state::lock(&self.entries);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(lines.as_bytes())?;
        file.sync_data()?;
        for record in records {
            Self::index(&mut entries, record);
        }
        Ok(())
    }

    /// Stable id of one phrase of one voice session in this process.
    pub fn id(&self, generation: u64, phrase: u64) -> String {
        format!("{}-{generation}-{phrase}", self.run)
    }

    /// On disk before this returns; call before any paste of the phrase.
    pub fn phrase(&self, id: &str, text: &str) -> std::io::Result<()> {
        self.append(&[Record::Phrase {
            id: id.into(),
            at: now_ms(),
            text: text.into(),
        }])
    }

    pub fn delivered(&self, id: &str) -> std::io::Result<()> {
        self.append(&[Record::Delivered {
            id: id.into(),
            at: now_ms(),
        }])
    }

    pub fn failed(&self, id: &str, reason: &str) -> std::io::Result<()> {
        self.append(&[Record::Failed {
            id: id.into(),
            at: now_ms(),
            reason: reason.into(),
        }])
    }

    pub fn copied(&self, ids: &[String]) -> std::io::Result<()> {
        let at = now_ms();
        let records: Vec<_> = ids
            .iter()
            .map(|id| Record::Copied { id: id.clone(), at })
            .collect();
        self.append(&records)
    }

    /// Oldest first: failed phrases, and phrases of earlier runs that never
    /// got an outcome. This run's unsettled phrases are still being pasted.
    pub fn waiting(&self) -> Vec<Waiting> {
        let prefix = format!("{}-", self.run);
        super::state::lock(&self.entries)
            .iter()
            .filter(|e| !e.done && (e.settled || !e.waiting.id.starts_with(&prefix)))
            .map(|e| e.waiting.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("diktor-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn voice_journal_phrase_is_on_disk_before_any_outcome() {
        let dir = directory("write");
        let journal = Journal::open_at(&dir, 1_000).unwrap();
        let id = journal.id(1, 1);
        journal.phrase(&id, "первая фраза").unwrap();
        let on_disk = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(on_disk.contains("первая фраза"));
        // In flight in this run: not offered for rescue yet.
        assert!(journal.waiting().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_journal_marks_delivered_failed_and_copied() {
        let dir = directory("mark");
        let journal = Journal::open_at(&dir, 1_000).unwrap();
        let (a, b, c) = (journal.id(1, 1), journal.id(1, 2), journal.id(1, 3));
        journal.phrase(&a, "доставлена").unwrap();
        journal.delivered(&a).unwrap();
        journal.phrase(&b, "не вставилась").unwrap();
        journal.failed(&b, "target_changed").unwrap();
        journal.phrase(&c, "вторая неудача").unwrap();
        journal.failed(&c, "cancelled").unwrap();
        let waiting = journal.waiting();
        assert_eq!(
            waiting
                .iter()
                .map(|w| (w.text.as_str(), w.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                ("не вставилась", "target_changed"),
                ("вторая неудача", "cancelled")
            ]
        );
        journal.copied(&[b.clone(), c.clone()]).unwrap();
        assert!(journal.waiting().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_journal_recovers_failed_and_interrupted_after_restart() {
        let dir = directory("recover");
        {
            let journal = Journal::open_at(&dir, 1_000).unwrap();
            let (a, b) = (journal.id(4, 1), journal.id(4, 2));
            journal.phrase(&a, "упала вставка").unwrap();
            journal.failed(&a, "target_changed").unwrap();
            // Killed between the durable write and the paste.
            journal.phrase(&b, "процесс убит").unwrap();
        }
        // A crash can tear the last line; complete lines still count.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(FILE_NAME))
            .unwrap();
        file.write_all(b"{\"event\":\"delivered\",\"id\":\"1000-4-")
            .unwrap();
        drop(file);
        let journal = Journal::open_at(&dir, 2_000).unwrap();
        let waiting = journal.waiting();
        assert_eq!(
            waiting
                .iter()
                .map(|w| (w.text.as_str(), w.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                ("упала вставка", "target_changed"),
                ("процесс убит", "interrupted")
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_journal_retention_drops_old_and_excess_phrases() {
        let dir = directory("retention");
        {
            let journal = Journal::open_at(&dir, 1_000).unwrap();
            journal
                .append(&[
                    Record::Phrase {
                        id: "old".into(),
                        at: 1_000,
                        text: "старое".into(),
                    },
                    Record::Failed {
                        id: "old".into(),
                        at: 1_000,
                        reason: "target_changed".into(),
                    },
                ])
                .unwrap();
            for n in 0..KEEP_PHRASES + 2 {
                let id = format!("new-{n}");
                journal
                    .append(&[
                        Record::Phrase {
                            id: id.clone(),
                            at: KEEP_MS + 5_000,
                            text: format!("фраза {n}"),
                        },
                        Record::Delivered {
                            id,
                            at: KEEP_MS + 5_000,
                        },
                    ])
                    .unwrap();
            }
        }
        let journal = Journal::open_at(&dir, KEEP_MS + 10_000).unwrap();
        assert!(journal.waiting().is_empty());
        let records = read(&dir.join(FILE_NAME)).unwrap();
        let phrases = records
            .iter()
            .filter(|r| matches!(r, Record::Phrase { .. }))
            .count();
        assert_eq!(phrases, KEEP_PHRASES);
        assert!(records.iter().all(|r| r.id() != "old" && r.id() != "new-0"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
