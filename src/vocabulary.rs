//! The owner's vocabulary: canonical terms speech should come out as
//! (Hyprland, xhub, Claude Code...). Two sources: the manual list in
//! diktor.db (`vocabulary` table, seeded once, `diktor vocab add`) and the
//! names of project directories (`vocabulary_dirs` in the config), where
//! `xhub-devbot` is also matchable as "xhub devbot". The glossary maps what
//! the model hears to these; the terms themselves are what `diktor vocab
//! list` shows and what the accuracy layer will match against.
use crate::journal::Journal;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Term {
    pub term: String,
    /// Spellings that also count as this term ("xhub devbot" for "xhub-devbot").
    pub forms: Vec<String>,
    /// `owner`, `claude`, or `dir:<path>`.
    pub source: String,
}

fn forms(term: &str) -> Vec<String> {
    let mut forms = vec![term.to_string()];
    let spaced = term.replace(['-', '_'], " ");
    if spaced != term {
        forms.push(spaced);
    }
    forms
}

/// Directory names under each of `dirs`: no files, nothing starting with `.`
/// or `_`, nothing in `skip`; sorted, first directory wins on duplicates.
pub fn project_terms(dirs: &[PathBuf], skip: &[String]) -> Vec<Term> {
    let mut found = BTreeMap::new();
    for directory in dirs {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !kind.is_dir()
                || name.starts_with(['.', '_'])
                || skip
                    .iter()
                    .any(|generic| generic.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            found.entry(name.to_lowercase()).or_insert_with(|| Term {
                forms: forms(&name),
                source: format!("dir:{}", entry.path().display()),
                term: name,
            });
        }
    }
    found.into_values().collect()
}

/// Manual terms first (they win on duplicates), then project directories.
pub fn effective(
    journal: &Journal,
    dirs: &[PathBuf],
    skip: &[String],
) -> anyhow::Result<Vec<Term>> {
    let mut terms: Vec<Term> = journal
        .vocabulary()?
        .into_iter()
        .map(|row| Term {
            forms: forms(&row.term),
            term: row.term,
            source: row.source,
        })
        .collect();
    for term in project_terms(dirs, skip) {
        if !terms
            .iter()
            .any(|known| known.term.eq_ignore_ascii_case(&term.term))
        {
            terms.push(term);
        }
    }
    Ok(terms)
}

/// The fuzzy index over the effective vocabulary, rebuilt when the manual
/// list changes (table version) or every few minutes (project directories).
pub struct Live {
    journal: std::sync::Arc<Journal>,
    dirs: Vec<PathBuf>,
    skip: Vec<String>,
    loaded: std::sync::Mutex<Option<(i64, std::time::Instant, crate::fuzzy::Index)>>,
}

const RESCAN: std::time::Duration = std::time::Duration::from_secs(300);

impl Live {
    pub fn new(journal: std::sync::Arc<Journal>, dirs: Vec<PathBuf>, skip: Vec<String>) -> Self {
        Self {
            journal,
            dirs,
            skip,
            loaded: std::sync::Mutex::new(None),
        }
    }

    pub fn apply(&self, text: &str) -> String {
        let version = self.journal.vocabulary_version().unwrap_or(-1);
        let mut loaded = crate::voice_session::state::lock(&self.loaded);
        let stale = loaded
            .as_ref()
            .is_none_or(|(v, at, _)| *v != version || at.elapsed() > RESCAN);
        if stale {
            match effective(&self.journal, &self.dirs, &self.skip) {
                Ok(terms) => {
                    log::info!("Vocabulary: {} terms for the fuzzy layer", terms.len());
                    *loaded = Some((
                        version,
                        std::time::Instant::now(),
                        crate::fuzzy::Index::new(&terms),
                    ));
                }
                Err(error) => log::warn!("Vocabulary not loaded: {error:#}"),
            }
        }
        match loaded.as_ref() {
            Some((_, _, index)) => index.apply(text),
            None => text.to_string(),
        }
    }
}

/// `~` expanded.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => Path::new(path).to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_directories_become_terms_with_spaced_forms() {
        let root = std::env::temp_dir().join(format!("diktor-vocab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["xhub-devbot", "aurelius", ".git", "_old", "src", "tg_mcp"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("notes.md"), "").unwrap();
        let terms = project_terms(std::slice::from_ref(&root), &["src".to_string()]);
        let names: Vec<(&str, Vec<&str>)> = terms
            .iter()
            .map(|t| {
                (
                    t.term.as_str(),
                    t.forms.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("aurelius", vec!["aurelius"]),
                ("tg_mcp", vec!["tg_mcp", "tg mcp"]),
                ("xhub-devbot", vec!["xhub-devbot", "xhub devbot"]),
            ]
        );
        assert!(terms[0].source.starts_with("dir:"));

        let db = std::env::temp_dir().join(format!("diktor-vocab-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&db);
        let journal = Journal::open(&db, 30, 1000).unwrap();
        journal.vocabulary_add("Aurelius", "claude", None).unwrap();
        let all = effective(&journal, std::slice::from_ref(&root), &["src".to_string()]).unwrap();
        let aurelius: Vec<&Term> = all
            .iter()
            .filter(|t| t.term.eq_ignore_ascii_case("aurelius"))
            .collect();
        // The seed has "aurelius" (owner); the manual list wins over the directory.
        assert_eq!(aurelius.len(), 1);
        assert!(!aurelius[0].source.starts_with("dir:"));
        assert!(all
            .iter()
            .any(|t| t.term == "Hyprland" && t.source == "owner"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(db).unwrap();
    }
}
