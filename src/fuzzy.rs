//! Second, fuzzy layer after the glossary: Cyrillic words (and runs of up to
//! three) that sound like one of the owner's Latin vocabulary terms are
//! written as that term. transcribe-cpp offers no hotword biasing for this
//! model family (only Whisper's `initial_prompt`), so the model hears
//! "Hyprland" and may write «Хайпрленд»; this puts it back.
//!
//! A candidate is transliterated, normalised the way English spelling maps
//! to sound (silent final e, c/k, y/i, doubled letters...) and reduced to a
//! skeleton: consonant classes with voicing merged, every vowel run one
//! marker. It becomes the term only when the skeletons are equal (or one
//! vowel marker apart for long terms) and the skeleton carries at least
//! three consonants; a Russian case ending on the last word is kept after
//! an apostrophe, as the glossary's stems do. Ordinary Russian words are the
//! risk, so the thresholds are strict and tested against a list of them.
use crate::glossary::ENDINGS;
use crate::vocabulary::Term;

fn translit(word: &str) -> String {
    let mut out = String::new();
    for c in word.to_lowercase().chars() {
        if c.is_ascii_alphabetic() {
            out.push(c);
            continue;
        }
        out.push_str(match c {
            'а' | 'я' => "a",
            'б' => "b",
            'в' => "v",
            'г' => "g",
            'д' => "d",
            'е' | 'ё' | 'э' => "e",
            'ж' => "zh",
            'з' => "z",
            'и' | 'й' | 'ы' => "i",
            'к' => "k",
            'л' => "l",
            'м' => "m",
            'н' => "n",
            'о' => "o",
            'п' => "p",
            'р' => "r",
            'с' => "s",
            'т' => "t",
            'у' | 'ю' => "u",
            'ф' => "f",
            'х' => "h",
            'ц' => "ts",
            'ч' => "ch",
            'ш' | 'щ' => "sh",
            _ => "",
        });
    }
    out
}

/// English spelling toward sound, lowercase ASCII letters only.
fn normalise(latin: &str) -> String {
    let lower: String = latin
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphabetic)
        .collect();
    let mut s = lower
        .replace("ph", "f")
        .replace("ck", "k")
        .replace("qu", "kv")
        .replace('q', "k")
        .replace('x', "ks")
        .replace('w', "v")
        .replace('y', "i")
        .replace("th", "t")
        .replace("gh", "g")
        .replace("oo", "u")
        .replace("ee", "i")
        .replace("ea", "i");
    // c: "ch" stays, before e/i it is s, else k.
    let chars: Vec<char> = s.chars().collect();
    s = chars
        .iter()
        .enumerate()
        .map(|(i, &c)| match (c, chars.get(i + 1)) {
            ('c', Some('h')) => 'c',
            ('c', Some('e' | 'i')) => 's',
            ('c', _) => 'k',
            (c, _) => c,
        })
        .collect();
    // Silent final e after a consonant ("claude", "fable", "rebase").
    if s.len() > 3 && s.ends_with('e') {
        let before = s.chars().rev().nth(1);
        if before.is_some_and(|c| !"aeiou".contains(c)) {
            s.pop();
        }
    }
    let mut collapsed = String::new();
    for c in s.chars() {
        if !collapsed.ends_with(c) {
            collapsed.push(c);
        }
    }
    collapsed
}

/// Consonant classes (voicing merged) and one marker per vowel run.
fn skeleton(norm: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = norm.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let (class, step) = match (c, next) {
            ('s' | 'z' | 'c', Some('h')) => ('S', 2),
            ('k', Some('h')) => ('h', 2),
            ('t', Some('s')) => ('s', 2),
            ('a' | 'e' | 'i' | 'o' | 'u', _) => ('a', 1),
            ('b' | 'p', _) => ('p', 1),
            ('d' | 't', _) => ('t', 1),
            ('g' | 'k', _) => ('k', 1),
            ('v' | 'f', _) => ('f', 1),
            ('z' | 's', _) => ('s', 1),
            (other, _) => (other, 1),
        };
        if !(class == 'a' && out.ends_with('a')) {
            out.push(class);
        }
        i += step;
    }
    out
}

fn consonants(skeleton: &str) -> usize {
    skeleton.chars().filter(|&c| c != 'a').count()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current.push(
                (previous[j] + cost)
                    .min(previous[j + 1] + 1)
                    .min(current[j] + 1),
            );
        }
        previous = current;
    }
    previous[b.len()]
}

struct Entry {
    written: String,
    words: usize,
    skeleton: String,
}

pub struct Index {
    entries: Vec<Entry>,
}

impl Index {
    /// Latin forms of the vocabulary; Cyrillic terms need no help.
    pub fn new(terms: &[Term]) -> Self {
        let mut entries = Vec::new();
        for term in terms {
            for form in &term.forms {
                if !form.chars().any(|c| c.is_ascii_alphabetic())
                    || form.chars().any(|c| !(c.is_ascii() || c == ' '))
                {
                    continue;
                }
                let skeleton = skeleton(&normalise(form));
                if consonants(&skeleton) < 3 {
                    continue;
                }
                entries.push(Entry {
                    written: term.term.clone(),
                    words: form.split_whitespace().count().clamp(1, 3),
                    skeleton,
                });
            }
        }
        Self { entries }
    }

    fn matches(&self, candidate: &str, words: usize) -> Option<&str> {
        // Short Russian words collide too easily («клад», «морг»).
        if candidate.chars().filter(|c| c.is_alphabetic()).count() < 5 {
            return None;
        }
        let skeleton = skeleton(&normalise(&translit(candidate)));
        if consonants(&skeleton) < 3 {
            return None;
        }
        self.entries
            .iter()
            // Two heard words may be one term («Хайпер Ланд»), not the reverse.
            .filter(|entry| entry.words == words || (entry.words == 1 && words == 2))
            .find(|entry| {
                entry.skeleton == skeleton
                    // One extra vowel only where two heard words make one term
                    // («Хайпер Ланд»): a single word gets no tolerance (релиз ≠ aurelius).
                    || (words == 2 && entry.words == 1 && entry.skeleton.len() >= 6
                        && levenshtein(&entry.skeleton, &skeleton) == 1
                        && entry.skeleton.replace('a', "") == skeleton.replace('a', ""))
            })
            .map(|entry| entry.written.as_str())
    }

    /// Replaces Cyrillic runs that sound like a vocabulary term.
    pub fn apply(&self, text: &str) -> String {
        if self.entries.is_empty() {
            return text.to_string();
        }
        let tokens = tokenize(text);
        let mut out = String::new();
        let mut i = 0;
        while i < tokens.len() {
            let mut replaced = false;
            if tokens[i].cyrillic {
                for words in (1..=3).rev() {
                    let Some(run) = cyrillic_run(&tokens, i, words) else {
                        continue;
                    };
                    let last = &tokens[run.end - 1].text;
                    let joined: Vec<&str> = tokens[i..run.end]
                        .iter()
                        .filter(|t| t.cyrillic)
                        .map(|t| t.text.as_str())
                        .collect();
                    let whole = joined.join(" ");
                    let found = self
                        .matches(&whole, words)
                        .map(|t| (t, String::new()))
                        .or_else(|| {
                            let lower = last.to_lowercase();
                            ENDINGS.iter().find_map(|ending| {
                                let base = lower.strip_suffix(ending)?;
                                if base.chars().count() < 5 {
                                    return None;
                                }
                                let prefix = &whole[..whole.len() - last.len()];
                                let candidate = format!("{prefix}{base}");
                                self.matches(&candidate, words)
                                    .map(|t| (t, format!("'{ending}")))
                            })
                        });
                    if let Some((term, ending)) = found {
                        out.push_str(term);
                        out.push_str(&ending);
                        i = run.end;
                        replaced = true;
                        break;
                    }
                }
            }
            if !replaced {
                out.push_str(&tokens[i].text);
                i += 1;
            }
        }
        out
    }
}

struct Token {
    text: String,
    cyrillic: bool,
}

/// Words and the separators between them, in order.
fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    for c in text.chars() {
        let cyrillic = ('\u{0400}'..='\u{04FF}').contains(&c);
        let word = c.is_alphanumeric();
        match tokens.last_mut() {
            Some(last) if last.cyrillic && cyrillic => last.text.push(c),
            Some(last)
                if !last.cyrillic && !word && !last.text.chars().any(char::is_alphanumeric) =>
            {
                last.text.push(c)
            }
            Some(last)
                if !last.cyrillic
                    && word
                    && !cyrillic
                    && last.text.chars().all(char::is_alphanumeric) =>
            {
                last.text.push(c)
            }
            _ => tokens.push(Token {
                text: c.to_string(),
                cyrillic,
            }),
        }
    }
    tokens
}

/// `words` Cyrillic words from `start`, separated by single spaces.
fn cyrillic_run(tokens: &[Token], start: usize, words: usize) -> Option<std::ops::Range<usize>> {
    let mut end = start;
    for n in 0..words {
        let word = tokens.get(end)?;
        if !word.cyrillic {
            return None;
        }
        end += 1;
        if n + 1 < words {
            let gap = tokens.get(end)?;
            if gap.text != " " {
                return None;
            }
            end += 1;
        }
    }
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(t: &str) -> Term {
        let mut forms = vec![t.to_string()];
        let spaced = t.replace(['-', '_'], " ");
        if spaced != t {
            forms.push(spaced);
        }
        Term {
            term: t.into(),
            forms,
            source: "owner".into(),
        }
    }

    fn owner() -> Index {
        let terms: Vec<Term> = [
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
            "agent-bus",
            "aurlune",
            "codebase-memory-mcp",
            "deepseek-harness",
            "diktor",
            "nexalix",
            "osiris",
            "stargeist",
            "tg-mcp",
            "timeforged",
            "vaultwire",
            "zal",
            "xhub-devbot",
        ]
        .iter()
        .map(|t| term(t))
        .collect();
        Index::new(&terms)
    }

    #[test]
    fn terms_heard_as_russian_are_written_as_terms() {
        let g = owner();
        assert_eq!(g.apply("открой хайпрленд"), "открой Hyprland");
        assert_eq!(g.apply("спроси клауд"), "спроси Claude");
        assert_eq!(g.apply("голос фейбл"), "голос Fable");
        assert_eq!(g.apply("сделай ребейз и деплой"), "сделай rebase и deploy");
        assert_eq!(g.apply("новый бранч"), "новый branch");
        assert_eq!(g.apply("это немотрон"), "это Nemotron");
        assert_eq!(g.apply("в телеграм"), "в Telegram");
        assert_eq!(g.apply("спроси клауда"), "спроси Claude'а");
        assert_eq!(g.apply("пул реквест"), "pull request");
        assert_eq!(g.apply("Хайпер Ланд упал"), "Hyprland упал");
    }

    #[test]
    fn ordinary_russian_words_stay() {
        let g = owner();
        let words = "клад клада склад колода схема вход сход работаем постоянно команда сервер \
            конфиг клавиатура класс плюс пуск мама папка ветка сделай задача проект запуск \
            проверка связи привет спасибо хорошо сегодня завтра вчера слово дело время человек \
            жизнь работа город страна деньги вопрос ответ голос память программа система \
            модель данные запись файл папку окно экран кнопка мышка курсор строка текст \
            письмо почта сайт ссылка телефон компьютер ноутбук монитор клавиша пароль логин \
            ошибка баг фича релиз версия сборка тест проверить исправить добавить удалить \
            открыть закрыть сохранить отправить получить найти искать думать знать делать \
            говорить писать читать слушать смотреть видеть хотеть мочь должен нужно можно \
            нельзя быстро медленно громко тихо прямо сейчас потом всегда никогда иногда \
            деплоить мержить коммитить пушить хаба клауза клавиша фабрика опера кокос \
            телега битва гитара диктовка осина залив агент автобус мастер вечер ребята";
        for word in words.split_whitespace() {
            assert_eq!(g.apply(word), word, "{word} must stay Russian");
        }
    }

    #[test]
    fn latin_text_punctuation_and_case_survive() {
        let g = owner();
        assert_eq!(g.apply("Hyprland, Claude: ok!"), "Hyprland, Claude: ok!");
        assert_eq!(g.apply("Хайпрленд, клауд."), "Hyprland, Claude.");
        assert_eq!(Index::new(&[]).apply("хайпрленд"), "хайпрленд");
    }

    #[test]
    fn skeletons_merge_voicing_and_vowels() {
        assert_eq!(
            skeleton(&normalise("Claude")),
            skeleton(&normalise(&translit("клауд")))
        );
        assert_eq!(
            skeleton(&normalise("Hyprland")),
            skeleton(&normalise(&translit("хайпрленд")))
        );
        assert_ne!(
            skeleton(&normalise("Claude")),
            skeleton(&normalise(&translit("колода")))
        );
    }
}
