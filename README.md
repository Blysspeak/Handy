# Diktor

Голосовой ввод для Hyprland одним бинарём без окон: модель распознаёт речь в
реальном времени, каждая фраза после ~1 с тишины вставляется в окно в фокусе,
после паузы — один Enter. Каждая фраза сначала пишется в базу, потом
вставляется; то, что не удалось вставить, можно забрать из бара или командой.

Выросло из форка [Handy](https://github.com/cjpais/Handy) (MIT): взяты захват
звука, VAD, потоковое распознавание transcribe-cpp и логика голосового режима.
Интерфейс Handy, трей, история, обновления, диктовка по кнопке и webview
удалены.

## Команды

```
diktor daemon            # демон (его запускает systemd-юнит)
diktor toggle            # включить / выключить голосовой режим
diktor off               # выключить (ничего, если уже выключен)
diktor status            # демон, модель, режим, RSS, сколько не вставлено
diktor copy [N]          # в буфер: невставленный текст из попапа бара,
                         # или последние N невставленных фраз
diktor dismiss           # закрыть попап бара (фразы остаются в базе)
diktor lang ru|en|auto   # язык распознавания, со следующей фразы; сохраняется в конфиг
diktor model list        # установленные и каталог потоковых моделей
diktor model download nemotron-3.5-asr-streaming-0.6b-gguf [--quant Q4_K_M]
diktor model use <file>  # переключить модель (голосовой режим должен быть выключен)
diktor panel [open|close|toggle]   # панель Diktor в баре (средний клик по иконке)
diktor devices           # имена микрофонов для input_device
```

Словарь и журнал (работают без демона, у каждой команды есть `--json`):

```
diktor glossary list                          # что как пишется
diktor glossary add камит commit              # точное слово
diktor glossary add --stem "икс хаб" xhub     # имя: падежи и приклеенный предлог
diktor glossary remove камит
diktor glossary approve гпт                   # принять предложение Claude (claude_glossary = "suggest")
diktor misses [--days 30]                     # латиница/КАПС в распознанном, чего нет в словаре
diktor rewrite "открой хайпрленд"             # что сделают словарь и нечёткий слой с текстом
diktor phrases recent [--limit 20]            # распознано / вставлено / статус
diktor phrases search камит
diktor phrases correct <id> "правильный текст"
diktor copy --id <id>                         # любую фразу из журнала в буфер
diktor vocab list [--source owner|claude|dir] # термины: ручные + имена каталогов проектов
diktor vocab add "Claude Code"
```

Словарь применяется к каждой готовой фразе до записи в базу и вставки;
распознанный текст хранится рядом (`phrases.raw_text`). Обычная запись
меняет только это слово целиком («коммит» → «commit», «коммиты» остаются).
Запись `--stem` — для имён: берёт падежные окончания (а у е ом ой ам ами ах
ы и) через апостроф и отрывает приклеенный предлог (с в к о): «иксхаба» →
«xhub'а», «Схабом» → «с xhub'ом». Регистр не важен, слова многословного
ключа совпадают через один пробел или дефис. Демон перечитывает словарь при
любом изменении таблицы, без перезапуска.

Второй слой (`fuzzy_terms = true`): кириллические слова, которые звучат как
латинский термин из словаря терминов (`diktor vocab list`: ручной список в
базе плюс имена каталогов из `vocabulary_dirs`), пишутся этим термином —
«хайпрленд» → Hyprland, «клауда» → Claude'а. Сравнение по звуковому скелету
(транслит, английское чтение, глухие/звонкие вместе, гласные — одной
меткой); слова короче 5 букв не трогаются, обычные русские слова проверены
тестом. Подсказку модели («hotwords») transcribe-cpp для этой модели не
даёт — только `initial_prompt` у Whisper.

## Файлы

| Что | Где |
|---|---|
| конфиг | `~/.config/diktor/config.toml` (пример: `contrib/config.example.toml`) |
| база | `~/.local/share/diktor/diktor.db` (SQLite: `phrases`, `glossary`, `vocabulary`) |
| сокет и состояние бара | `$XDG_RUNTIME_DIR/diktor/{diktor.sock,voice-state.json}` |
| юнит | `~/.config/systemd/user/diktor.service` (`contrib/systemd/`) |
| виджет бара Omarchy | `~/.config/omarchy/bar/modules/diktor.qml` (`contrib/omarchy/`) |

Модель по умолчанию — `nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf`, ищется в
`~/.local/share/diktor/models/`, затем (только чтение) в
`~/.local/share/com.pais.handy/models/`. Язык `auto` (модель сама определяет
язык каждой фразы), backend `auto` (Vulkan). Стартовый словарь —
`contrib/glossary.seed.toml`, загружается в базу один раз.

## Сборка и установка

```
TRANSCRIBE_CMAKE_ARGS=-DBLA_VENDOR=OpenBLAS CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2 scripts/install.sh
systemctl --user restart diktor
```

`install.sh` ставит сборку рядом с прежними в
`~/.local/opt/diktor/<дата>-<коммит>/` и переставляет `~/.local/bin/diktor`;
откат на прежнюю сборку — переставить симлинк и перезапустить юнит.

Проверки: `cargo test --release`, `cargo clippy --release --all-targets -- -D warnings`
(в `Cargo.toml` запрещены `unwrap`/`expect` вне тестов),
`python3 -m unittest` в `src/voice_session/`. Сквозные проверки без
микрофона: `diktor daemon --input-wav <file>` проигрывает WAV в тот же
конвейер захвата при каждом включении режима; `DIKTOR_ONLY_WINDOW=<адрес окна>`
запрещает вставку в любое другое окно; `DIKTOR_RUNTIME_DIR`, `DIKTOR_DATA_DIR`,
`DIKTOR_CONFIG` уводят сокет, базу и конфиг в сторону.
