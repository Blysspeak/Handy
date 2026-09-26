//! Script-friendly commands on diktor.db: glossary, vocabulary, phrases and
//! misses. They open the database directly (no daemon needed); `--json`
//! prints one JSON document on stdout. The daemon picks glossary changes up
//! from the next phrase.
use crate::config::{self, Config};
use crate::glossary;
use crate::journal::{self, Journal};
use crate::vocabulary;
use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Source {
    Owner,
    Claude,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Owner => "owner",
            Source::Claude => "claude",
        }
    }
}

#[derive(Subcommand)]
pub enum GlossaryAction {
    /// Add or replace an entry: `diktor glossary add --stem "икс хаб" xhub`.
    Add {
        spoken: String,
        written: String,
        /// Also Russian case endings and a glued preposition (names, products).
        #[arg(long)]
        stem: bool,
        #[arg(long, value_enum, default_value_t = Source::Owner)]
        source: Source,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove an entry (both kinds unless --stem or --word).
    Remove {
        spoken: String,
        #[arg(long, conflicts_with = "word")]
        stem: bool,
        #[arg(long)]
        word: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print the entries.
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum PhrasesAction {
    /// The newest phrases: raw (recognised), text (pasted), status.
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Store the right text for a phrase (a reference sample).
    Correct {
        id: i64,
        text: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum VocabAction {
    /// The effective vocabulary and where each term comes from.
    List {
        /// owner, claude or dir
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Add {
        term: String,
        #[arg(long, value_enum, default_value_t = Source::Owner)]
        source: Source,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Remove {
        term: String,
        #[arg(long)]
        json: bool,
    },
}

fn open() -> anyhow::Result<(Journal, Config)> {
    let config = Config::load(&config::config_path())?;
    let journal = Journal::open(&config::data_dir(), config.keep_days, config.keep_phrases)?;
    Ok((journal, config))
}

fn print_json(value: &impl Serialize) -> anyhow::Result<ExitCode> {
    println!("{}", serde_json::to_string(value)?);
    Ok(ExitCode::SUCCESS)
}

pub fn glossary_command(action: GlossaryAction) -> anyhow::Result<ExitCode> {
    let (journal, _) = open()?;
    match action {
        GlossaryAction::Add {
            spoken,
            written,
            stem,
            source,
            note,
            json,
        } => {
            anyhow::ensure!(
                !glossary::normalise(&spoken).is_empty() && !written.trim().is_empty(),
                "пустая запись"
            );
            journal.glossary_add(
                &spoken,
                written.trim(),
                stem,
                source.as_str(),
                note.as_deref(),
            )?;
            let from = glossary::normalise(&spoken);
            if json {
                return print_json(
                    &json!({ "ok": true, "from": from, "to": written.trim(), "stem": stem }),
                );
            }
            println!(
                "{from} -> {} ({})",
                written.trim(),
                if stem { "stem" } else { "word" }
            );
        }
        GlossaryAction::Remove {
            spoken,
            stem,
            word,
            json,
        } => {
            let kind = match (stem, word) {
                (true, _) => Some("stem"),
                (_, true) => Some("word"),
                _ => None,
            };
            let removed = journal.glossary_remove(&spoken, kind)?;
            if json {
                return print_json(&json!({ "ok": removed > 0, "removed": removed }));
            }
            if removed == 0 {
                eprintln!("diktor: нет такой записи: {}", glossary::normalise(&spoken));
                return Ok(ExitCode::FAILURE);
            }
            println!("удалено: {removed}");
        }
        GlossaryAction::List { json } => {
            let rows = journal.glossary_rows()?;
            if json {
                return print_json(&rows);
            }
            for row in rows {
                println!(
                    "{}{} -> {}  [{}{}{}]",
                    if row.kind == "stem" { "~" } else { "" },
                    row.from,
                    row.to,
                    row.source,
                    row.note.map(|n| format!(", {n}")).unwrap_or_default(),
                    if row.enabled { "" } else { ", off" }
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn phrases_command(action: PhrasesAction) -> anyhow::Result<ExitCode> {
    let (journal, _) = open()?;
    match action {
        PhrasesAction::Recent { limit, json } => {
            let rows = journal.recent(limit)?;
            if json {
                return print_json(&rows);
            }
            for row in rows {
                let changed = if row.raw != row.text {
                    format!("  (распознано: {})", row.raw)
                } else {
                    String::new()
                };
                println!(
                    "#{} {} {}: {}{changed}",
                    row.id, row.created_at, row.status, row.text
                );
            }
        }
        PhrasesAction::Correct { id, text, json } => {
            let found = journal.correct(id, &text)?;
            if json {
                return print_json(&json!({ "ok": found, "id": id }));
            }
            if !found {
                eprintln!("diktor: нет фразы #{id}");
                return Ok(ExitCode::FAILURE);
            }
            println!("#{id}: исправлено");
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn vocab_command(action: VocabAction) -> anyhow::Result<ExitCode> {
    let (journal, config) = open()?;
    match action {
        VocabAction::List { source, json } => {
            let dirs: Vec<_> = config
                .vocabulary_dirs
                .iter()
                .map(|d| vocabulary::expand(d))
                .collect();
            let terms: Vec<vocabulary::Term> =
                vocabulary::effective(&journal, &dirs, &config.vocabulary_skip)?
                    .into_iter()
                    .filter(|t| source.as_deref().is_none_or(|s| t.source.starts_with(s)))
                    .collect();
            if json {
                return print_json(&terms);
            }
            for term in terms {
                println!("{}  [{}]", term.forms.join(" | "), term.source);
            }
        }
        VocabAction::Add {
            term,
            source,
            note,
            json,
        } => {
            anyhow::ensure!(!term.trim().is_empty(), "пустой термин");
            journal.vocabulary_add(&term, source.as_str(), note.as_deref())?;
            if json {
                return print_json(&json!({ "ok": true, "term": term.trim() }));
            }
            println!("{}", term.trim());
        }
        VocabAction::Remove { term, json } => {
            let removed = journal.vocabulary_remove(&term)?;
            if json {
                return print_json(&json!({ "ok": removed > 0, "removed": removed }));
            }
            if removed == 0 {
                eprintln!("diktor: нет такого термина: {}", term.trim());
                return Ok(ExitCode::FAILURE);
            }
            println!("удалено: {removed}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct MissJson {
    token: String,
    count: usize,
    example: String,
}

pub fn misses_command(days: u32, json: bool) -> anyhow::Result<ExitCode> {
    let (journal, _) = open()?;
    let since = journal::now_ms() - i64::from(days) * 24 * 60 * 60 * 1000;
    let texts = journal.recognised_since(since)?;
    let misses = glossary::misses(&texts, &journal.glossary()?);
    if json {
        let misses: Vec<MissJson> = misses
            .into_iter()
            .map(|m| MissJson {
                token: m.token,
                count: m.count,
                example: m.example,
            })
            .collect();
        return print_json(&misses);
    }
    if misses.is_empty() {
        println!("пропусков нет ({} фраз за {days} дн.)", texts.len());
    }
    for miss in misses {
        println!("{:>4}  {}  «{}»", miss.count, miss.token, miss.example);
    }
    Ok(ExitCode::SUCCESS)
}
