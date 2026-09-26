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
diktor devices           # имена микрофонов для input_device
```

## Файлы

| Что | Где |
|---|---|
| конфиг | `~/.config/diktor/config.toml` (пример: `contrib/config.example.toml`) |
| база фраз | `~/.local/share/diktor/diktor.db` (SQLite, таблица `phrases`) |
| сокет и состояние бара | `$XDG_RUNTIME_DIR/diktor/{diktor.sock,voice-state.json}` |
| юнит | `~/.config/systemd/user/diktor.service` (`contrib/systemd/`) |
| виджет бара Omarchy | `~/.config/omarchy/bar/modules/diktor.qml` (`contrib/omarchy/`) |

Модель по умолчанию — `nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf`, ищется в
`~/.local/share/diktor/models/`, затем (только чтение) в
`~/.local/share/com.pais.handy/models/`. Язык `ru`, backend `auto` (Vulkan).

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
