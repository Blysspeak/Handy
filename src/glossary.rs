//! Personal glossary, stored in diktor.db (table `glossary`, seeded once from
//! contrib/glossary.seed.toml): spoken form (as the model writes it) =
//! written form. Applied to every final phrase before it is journaled and
//! pasted; the raw text is kept in the journal too.
//!
//! Top-level entries are exact words: `"коммит" = "commit"` changes «коммит»
//! but not «коммиты». Entries under `[stems]` are names Russian inflects:
//! `"иксхаб" = "xhub"` also takes a case ending (а у е ом ой ам ами ах ы и),
//! written after an apostrophe, and a single-letter preposition the model
//! glued on (с в к о) is split off: «иксхаба» → «xhub'а», «Схабом» →
//! «с xhub'ом» (with `"хаб"` as a stem). Both kinds match whole words,
//! case-insensitively; the words of a multi-word key match across one space
//! or hyphen. A replacement at the start of a sentence keeps the capital
//! letter the spoken word had. The daemon re-reads the table whenever its
//! version (bumped by triggers) changed, so `diktor glossary add` takes
//! effect from the next phrase without a restart.
use crate::journal::Journal;
use crate::voice_session::state::lock;
use regex::{Captures, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Russian case endings a `[stems]` entry may take, longest first.
pub const ENDINGS: [&str; 10] = ["ами", "ам", "ах", "ом", "ой", "а", "у", "е", "ы", "и"];
/// Prepositions the model glues onto the next word («с хабом» → «схабом»).
const GLUED_PREPOSITIONS: &str = "свко";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entries {
    #[serde(flatten)]
    pub words: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stems: BTreeMap<String, String>,
}

/// Lowercase, words separated by single spaces: how keys are stored and looked up.
pub fn normalise(spoken: &str) -> String {
    spoken
        .split(|c: char| c.is_whitespace() || c == '-')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Normalised keys, longest first, as one regex alternation whose words
/// match across one space or hyphen.
fn alternation(map: &BTreeMap<String, String>) -> Option<String> {
    let mut spoken: Vec<&String> = map.keys().collect();
    spoken.sort_by_key(|from| std::cmp::Reverse(from.chars().count()));
    (!spoken.is_empty()).then(|| {
        spoken
            .iter()
            .map(|from| {
                from.split(' ')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(r"[\s\-]")
            })
            .collect::<Vec<_>>()
            .join("|")
    })
}

fn normalised(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    map.iter()
        .map(|(from, to)| (normalise(from), to.clone()))
        .filter(|(from, _)| !from.is_empty())
        .collect()
}

fn build(pattern: &str) -> Result<Regex, regex::Error> {
    // \b is Unicode-aware: Cyrillic and Latin letters are word characters.
    RegexBuilder::new(pattern).case_insensitive(true).build()
}

struct Compiled {
    words: Option<Regex>,
    word_map: BTreeMap<String, String>,
    stems: Option<Regex>,
    stem_map: BTreeMap<String, String>,
}

impl Compiled {
    fn new(entries: &Entries) -> Result<Self, regex::Error> {
        let word_map = normalised(&entries.words);
        let stem_map = normalised(&entries.stems);
        let words = alternation(&word_map)
            .map(|alternation| build(&format!(r"\b(?:{alternation})\b")))
            .transpose()?;
        let stems = alternation(&stem_map)
            .map(|alternation| {
                build(&format!(
                    r"\b(?P<prep>[{GLUED_PREPOSITIONS}]\s?)?(?P<stem>{alternation})(?P<end>{})?\b",
                    ENDINGS.join("|")
                ))
            })
            .transpose()?;
        Ok(Self {
            words,
            word_map,
            stems,
            stem_map,
        })
    }

    fn apply(&self, text: &str) -> String {
        let text = match &self.words {
            Some(words) => words
                .replace_all(text, |captures: &Captures| {
                    let found = captures.get(0).map_or("", |m| m.as_str());
                    let start = captures.get(0).map_or(0, |m| m.start());
                    match self.word_map.get(&normalise(found)) {
                        Some(written) => cased(found, written, sentence_start(&text[..start])),
                        None => found.to_string(),
                    }
                })
                .into_owned(),
            None => text.to_string(),
        };
        let Some(stems) = &self.stems else {
            return text;
        };
        stems
            .replace_all(&text, |captures: &Captures| {
                let whole = captures.get(0).map_or("", |m| m.as_str());
                let start = captures.get(0).map_or(0, |m| m.start());
                let stem = captures.name("stem").map_or("", |m| m.as_str());
                let Some(written) = self.stem_map.get(&normalise(stem)) else {
                    return whole.to_string();
                };
                let at_start = sentence_start(&text[..start]);
                let ending = captures
                    .name("end")
                    .map(|m| format!("'{}", m.as_str().to_lowercase()))
                    .unwrap_or_default();
                match captures.name("prep") {
                    // A preposition in front: glued («схабом») or its own word
                    // («с хабом»); either way it stays a separate word.
                    Some(prep) => {
                        let letter = prep.as_str().trim();
                        let letter = if at_start && starts_upper(letter) {
                            letter.to_string()
                        } else {
                            letter.to_lowercase()
                        };
                        format!("{letter} {written}{ending}")
                    }
                    None => format!("{}{ending}", cased(stem, written, at_start)),
                }
            })
            .into_owned()
    }
}

fn starts_upper(word: &str) -> bool {
    word.chars().next().is_some_and(char::is_uppercase)
}

/// The written form, capitalised only at a sentence start where the spoken
/// word was capitalised.
fn cased(spoken: &str, written: &str, at_sentence_start: bool) -> String {
    if at_sentence_start && starts_upper(spoken) {
        capitalise(written)
    } else {
        written.to_string()
    }
}

/// Nothing but spaces (and opening quotes/brackets) since the text start or
/// the last sentence end.
fn sentence_start(before: &str) -> bool {
    let trimmed = before.trim_end_matches(|c: char| c.is_whitespace() || "«\"'(„“[".contains(c));
    trimmed.is_empty() || trimmed.ends_with(['.', '!', '?', '…'])
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A token in recognised text that looks like a mangled term.
#[derive(Debug, PartialEq, Eq)]
pub struct Miss {
    pub token: String,
    pub count: usize,
    pub example: String,
}

/// `diktor glossary misses`: all-caps Latin runs of 3+ letters, and Latin
/// words inside otherwise Cyrillic phrases, that the glossary does not know
/// yet (neither as a spoken nor as a written form); most frequent first.
pub fn misses(texts: &[String], entries: &Entries) -> Vec<Miss> {
    let mut known = std::collections::BTreeSet::new();
    for (from, to) in entries.words.iter().chain(&entries.stems) {
        for form in [normalise(from), normalise(to)] {
            known.extend(form.split(' ').map(str::to_string));
            known.insert(form);
        }
    }
    let (Ok(latin), Ok(caps), Ok(cyrillic)) = (
        Regex::new(r"\b[A-Za-z][A-Za-z0-9]*\b"),
        Regex::new(r"^[A-Z]{3,}$"),
        Regex::new(r"\p{Cyrillic}"),
    ) else {
        return Vec::new();
    };
    let mut found: BTreeMap<String, Miss> = BTreeMap::new();
    for text in texts {
        let in_russian = cyrillic.is_match(text);
        for token in latin.find_iter(text).map(|m| m.as_str()) {
            let key = token.to_lowercase();
            if known.contains(&key) || !(caps.is_match(token) || in_russian) {
                continue;
            }
            found
                .entry(key)
                .or_insert_with(|| Miss {
                    token: token.to_string(),
                    count: 0,
                    example: text.chars().take(100).collect(),
                })
                .count += 1;
        }
    }
    let mut misses: Vec<Miss> = found.into_values().collect();
    misses.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.token.cmp(&b.token)));
    misses
}

pub struct Glossary {
    journal: Arc<Journal>,
    /// Table version the compiled entries come from.
    loaded: Mutex<(Option<i64>, Compiled)>,
}

impl Glossary {
    pub fn new(journal: Arc<Journal>) -> Self {
        let empty = Compiled {
            words: None,
            word_map: BTreeMap::new(),
            stems: None,
            stem_map: BTreeMap::new(),
        };
        let glossary = Self {
            journal,
            loaded: Mutex::new((None, empty)),
        };
        glossary.refresh();
        glossary
    }

    /// Re-reads the table when its version changed. A failure keeps the
    /// previous entries and logs why.
    fn refresh(&self) {
        let version = match self.journal.glossary_version() {
            Ok(version) => version,
            Err(error) => {
                log::warn!("Glossary version not read: {error}");
                return;
            }
        };
        let mut loaded = lock(&self.loaded);
        if loaded.0 == Some(version) {
            return;
        }
        let compiled = self
            .journal
            .glossary()
            .map_err(anyhow::Error::from)
            .and_then(|entries| Compiled::new(&entries).map_err(anyhow::Error::from));
        match compiled {
            Ok(compiled) => {
                log::info!(
                    "Glossary v{version}: {} words, {} stems",
                    compiled.word_map.len(),
                    compiled.stem_map.len()
                );
                *loaded = (Some(version), compiled);
            }
            Err(error) => {
                log::warn!("Glossary not reloaded: {error:#}");
                loaded.0 = Some(version);
            }
        }
    }

    pub fn apply(&self, text: &str) -> String {
        self.refresh();
        lock(&self.loaded).1.apply(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(words: &[(&str, &str)], stems: &[(&str, &str)]) -> Entries {
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        Entries {
            words: map(words),
            stems: map(stems),
        }
    }

    fn compiled(words: &[(&str, &str)], stems: &[(&str, &str)]) -> Compiled {
        Compiled::new(&entries(words, stems)).unwrap()
    }

    fn seed() -> Compiled {
        Compiled::new(&toml::from_str(include_str!("../contrib/glossary.seed.toml")).unwrap())
            .unwrap()
    }

    #[test]
    fn exact_words_only_across_cyrillic_and_latin() {
        let g = compiled(&[("коммит", "commit"), ("клод", "Claude")], &[]);
        assert_eq!(g.apply("сделай коммит и запушь"), "сделай commit и запушь");
        // Inflected or longer words are other words for a plain entry.
        assert_eq!(
            g.apply("все коммиты и перекоммит"),
            "все коммиты и перекоммит"
        );
        assert_eq!(g.apply("клодик"), "клодик");
        // Punctuation and hyphens are boundaries, digits are not.
        assert_eq!(g.apply("спроси клода, клод."), "спроси клода, Claude.");
        assert_eq!(g.apply("git-коммит"), "git-commit");
        assert_eq!(g.apply("коммит123"), "коммит123");
    }

    #[test]
    fn case_insensitive_and_sentence_start_capital() {
        let g = compiled(&[("коммит", "commit"), ("фейбл", "Fable")], &[]);
        assert_eq!(
            g.apply("Коммит готов. коммит второй"),
            "Commit готов. commit второй"
        );
        assert_eq!(g.apply("Готово! Коммит ушёл"), "Готово! Commit ушёл");
        // Mid-sentence capital from the model: the glossary spelling wins.
        assert_eq!(g.apply("это Коммит"), "это commit");
        assert_eq!(g.apply("ФЕЙБЛ и фейбл"), "Fable и Fable");
    }

    #[test]
    fn multi_word_keys_match_across_one_space_or_hyphen() {
        let g = compiled(
            &[("хайпер ланд", "Hyprland"), ("пул реквест", "pull request")],
            &[],
        );
        assert_eq!(g.apply("открой хайпер ланд"), "открой Hyprland");
        assert_eq!(g.apply("Хайпер-Ланд упал"), "Hyprland упал");
        assert_eq!(g.apply("в пул реквест"), "в pull request");
        // Two spaces or another word in between: not the key.
        assert_eq!(g.apply("хайпер  ланд"), "хайпер  ланд");
        assert_eq!(g.apply("хайпер большой ланд"), "хайпер большой ланд");
        assert_eq!(g.apply("хайперланд"), "хайперланд");
        assert_eq!(g.apply("хайпер ландыш"), "хайпер ландыш");
    }

    #[test]
    fn stems_take_case_endings_after_an_apostrophe() {
        let g = compiled(
            &[],
            &[
                ("иксхаб", "xhub"),
                ("клауд", "Claude"),
                ("хайпер ланд", "Hyprland"),
            ],
        );
        assert_eq!(g.apply("иксхаб"), "xhub");
        assert_eq!(g.apply("для иксхаба"), "для xhub'а");
        assert_eq!(g.apply("в иксхабе"), "в xhub'е");
        assert_eq!(g.apply("с иксхабом"), "с xhub'ом");
        assert_eq!(g.apply("по иксхабам и иксхабами"), "по xhub'ам и xhub'ами");
        assert_eq!(g.apply("в хайпер ланде"), "в Hyprland'е");
        assert_eq!(g.apply("спроси клауда"), "спроси Claude'а");
        // Endings outside the list, and longer words, stay.
        assert_eq!(g.apply("Клаудия и иксхабовский"), "Клаудия и иксхабовский");
        assert_eq!(g.apply("иксхабу"), "xhub'у");
        assert_eq!(g.apply("иксхабой"), "xhub'ой");
        assert_eq!(g.apply("иксхабах иксхабы иксхаби"), "xhub'ах xhub'ы xhub'и");
    }

    #[test]
    fn glued_preposition_is_split_off() {
        let g = compiled(&[], &[("хаб", "xhub"), ("иксхаб", "xhub")]);
        assert_eq!(g.apply("работаем Схабом"), "работаем с xhub'ом");
        assert_eq!(g.apply("Схабом работаем"), "С xhub'ом работаем");
        assert_eq!(g.apply("мы с хабом"), "мы с xhub'ом");
        assert_eq!(
            g.apply("вхабе и кхабу, охабе"),
            "в xhub'е и к xhub'у, о xhub'е"
        );
        assert_eq!(g.apply("сиксхабом"), "с xhub'ом");
        // Real words whose remainder is not a stem stay untouched.
        assert_eq!(
            g.apply("схема вход сход охапка хабар схабер"),
            "схема вход сход охапка хабар схабер"
        );
    }

    #[test]
    fn owner_seed_covers_the_forms_the_model_produced() {
        let g = seed();
        assert_eq!(
            g.apply("Фейбл, Хайпер Ланд, Клауд и Камит. XHPSP и XHAB"),
            "Fable, Hyprland, Claude и commit. Xhub и xhub"
        );
        assert_eq!(
            g.apply("мы постоянно работаем Схабом"),
            "мы постоянно работаем с xhub'ом"
        );
        assert_eq!(
            g.apply("иксхапа нет, в иксхабе есть"),
            "xhub'а нет, в xhub'е есть"
        );
        assert_eq!(g.apply("все коммиты"), "все коммиты");
    }

    #[test]
    fn misses_are_caps_runs_and_latin_inside_russian_not_yet_known() {
        let texts = [
            "запусти XHPSP на сервере".to_string(),
            "XHPSP опять упал".to_string(),
            "открой Hyprland и Xhab".to_string(),
            "проверь TODO в коде".to_string(),
            "The quick brown fox".to_string(),
            "OK".to_string(),
        ];
        let known = entries(&[("xhab", "xhub")], &[("хайпер ланд", "Hyprland")]);
        let misses = misses(&texts, &known);
        let found: Vec<(&str, usize)> =
            misses.iter().map(|m| (m.token.as_str(), m.count)).collect();
        // Hyprland and Xhab are known; English-only phrases and short caps are not misses.
        assert_eq!(found, [("XHPSP", 2), ("TODO", 1)]);
        assert_eq!(misses[0].example, "запусти XHPSP на сервере");
    }

    #[test]
    fn database_changes_apply_from_the_next_phrase_without_restart() {
        let directory =
            std::env::temp_dir().join(format!("diktor-glossary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let journal = Arc::new(Journal::open(&directory, 30, 1000).unwrap());
        let glossary = Glossary::new(Arc::clone(&journal));
        // Seeded on first open.
        assert_eq!(glossary.apply("работаем Схабом"), "работаем с xhub'ом");
        assert_eq!(glossary.apply("спроси гпт"), "спроси гпт");
        // A second connection (the CLI) writes; the daemon's glossary sees it.
        let cli = Journal::open(&directory, 30, 1000).unwrap();
        cli.glossary_add("гпт", "GPT", false, "claude", None)
            .unwrap();
        assert_eq!(glossary.apply("спроси гпт"), "спроси GPT");
        cli.glossary_remove("хаб", None).unwrap();
        assert_eq!(glossary.apply("мы с хабом"), "мы с хабом");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
