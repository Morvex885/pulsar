# AGENTS.md

pulsar — нативный Rust-демон вместо стека Яндекса (`maind`) на рутованных Яндекс Станциях: Home
Assistant через ESPHome Native API, Music Assistant через Sendspin, на Миди — Zigbee-радио.
Проприетарные библиотеки прошивки грузятся `dlopen` со станции и никогда не копируются в
репозиторий. Документация (`README.md`, `docs/`) — на русском.

## Харнесс

- Канон для агентов — этот файл и `.agents/skills/<name>/SKILL.md` (формат
  [agentskills.io](https://agentskills.io): frontmatter `name`, `description`).
- Инструменту, который не читает оригинал, добавляется только симлинк (сейчас `.claude/skills →
  ../.agents/skills`). Конфигов под конкретные инструменты в репозитории нет: не создавать
  `CLAUDE.md`, `.claude/settings.json`, `.codex/`, вложенные `AGENTS.md`.
- На Windows без `core.symlinks` симлинк не работает: читать `.agents/skills/` напрямую.
- `AGENTS.md` держать ≤ 16 KiB (у части агентов лимит чтения 32 KiB). Здесь только правила для любой
  задачи; новые факты идут в `docs/`.

| skill | когда |
|---|---|
| `station-test` | любое действие на живой станции: lock, `stop`, тестовый экземпляр, живая подмена, `cleanup` |
| `add-station-model` | новая модель станции: сбор данных read-only, измерения, `Model`, установщик, `boot.sh`, docs |
| `zigbee-radio` | сборка и прошивка образов радио Миди, `zigbee_probe`, чтение через `tools/deconz_read.py` |
| `crash-decode` | расшифровка `=== CRASH` из лога через `addr2line` и `pulsar.debug` |
| `release` | тег `v*` и GitHub release — только по явной просьбе пользователя |
| `pre-publish-review` | обязательно перед `git push`, `gh pr create` и тегом `v*` |

## Куда смотреть

| задача | doc | skill |
|---|---|---|
| код демона: крейты, модель исполнения, FMA, `state.conf`, порты | [docs/architecture.md](docs/architecture.md) | — |
| голос: HA voice assistant, стоковый maind, арбитраж, пре-ролл | [docs/voice.md](docs/voice.md) | — |
| железо, библиотеки Яндекса, новая модель | [docs/hardware.md](docs/hardware.md), [docs/mini3.md](docs/mini3.md) | `add-station-model` |
| Zigbee (Миди), прошивки `radio/` | [docs/zigbee.md](docs/zigbee.md) | `zigbee-radio` |
| сборка, CI, установщик, супервизор, обновления, релиз, падение | [docs/development.md](docs/development.md) | `release`, `crash-decode` |
| тесты на станциях | [docs/stations.md](docs/stations.md) | `station-test` |
| поведение для пользователя | [README.md](README.md) | — |

Каталоги: `satellite/` — [docs/architecture.md, «Workspace»](docs/architecture.md#workspace);
`installer/`, `satellite/deploy/`, `ssh/` — [docs/development.md](docs/development.md); `radio/` —
[docs/zigbee.md](docs/zigbee.md); `tools/` — [docs/stations.md](docs/stations.md) и skill
`station-test`.

## Сборка и проверки

```sh
make -C satellite -j"$(nproc)"                        # обе архитектуры
make -C satellite ARCH=armhf                          # одна архитектура
make -C satellite ARCH=armhf tools                    # все утилиты src/bin
make -C satellite ARCH=armhf build/armhf/tools/<name> # одна утилита
make -C satellite check                               # fmt, clippy -D warnings, тесты
go -C installer test ./...
radio/build.sh
ssh/build.sh
```

- Версии инструментов, выходы, версия бинаря и zig для `make check` —
  [docs/development.md, «Сборка»](docs/development.md#сборка).
- `make -C satellite check` должен проходить. Хостовые тесты покрывают Rust-крейты и установщик;
  поведение железа проверяется на станции (skill `station-test`).
- Перед `git push`, `gh pr create` и тегом `v*` пройти skill `pre-publish-review` до чистого раунда.

## Правила кода

- **Акторы и неблокирующие handle** — [docs/architecture.md, «Модель
  исполнения»](docs/architecture.md#модель-исполнения).
- **`unsafe` и libc** — только в модулях из
  [docs/architecture.md, «Модель исполнения»](docs/architecture.md#модель-исполнения), п. 8.
- **`mul_add` не добавлять и не убирать** — [«Плавающая точка
  (FMA)»](docs/architecture.md#плавающая-точка-fma).
- **`state.conf` совместим с `.prev`** — [«state.conf и откат на
  .prev»](docs/architecture.md#stateconf-и-откат-на-prev).
- **Зависимости** пинятся точной версией в `[workspace.dependencies]` (`satellite/Cargo.toml`);
  новая внешняя зависимость — только с обоснованием.
- **Код под архитектуру** целиком, со всеми хелперами, под `#[cfg(target_arch = …)]`: мёртвого кода
  на других целях нет (`clippy -D warnings` на хосте и обеих станционных целях).
- **Комментарии** объясняют факты устройства, ограничения и «почему», а не «что». Новые — на
  русском; комментарий, чей смысл меняется, переписывается на русском целиком; нетронутые не
  переводятся. Если обоснование — факт из docs, у кода одна строка `// см. docs/<файл>.md,
  «<раздел>»`.

## Документация

- Поведение кода в docs не пересказывать; обоснование решения — комментарием у кода.
- В docs только железо, внешние системы (HA, ESPHome, maind, библиотеки Яндекса, Zigbee-стек,
  GitHub), межкомпонентные контракты, правила и навигация.
- Каждый факт — в одном месте, остальные ссылаются (`docs/<файл>.md, «<раздел>»`; якоря должны
  разрешаться). Новый факт — в doc своей подсистемы; в `AGENTS.md` — только правила для любой
  задачи.
- По железу авторитетны Миди и Лайт 2 (станции мейнтейнера); по остальным моделям —
  [docs/hardware.md, «Источники»](docs/hardware.md#источники).

## Git

- `main` — единственная долгоживущая ветка, защищена ruleset: без прямых push, force push и
  удаления. Изменения — через PR с зелёными `build (aarch64)`, `build (armhf)` и `commits`; только
  squash или rebase (линейная история).
- Работа — в короткой ветке `feat/…`, `fix/…`, `docs/…`; ветка удаляется при мерже. Теги `v*` нельзя
  сдвинуть или удалить.
- Персональные данные не попадают в репозиторий: никаких реальных IP, MAC, серийников, имён Wi-Fi,
  токенов, логов станций. Плейсхолдер адреса — `192.168.1.50`.
- `backup/` (полные бэкапы станций), `dump/` (бинарники прошивок, логи), `re/` (проекты Ghidra и
  декомпилированный код Яндекса), `ref/` (upstream-чекауты) — в `.gitignore`, не публиковать.
  Проприетарные библиотеки, модели и конфиги прошивки — тоже.
- **Авторство.** Автор и коммиттер — человек, от чьего имени идёт работа, со своей настроенной git
  identity. Агент не меняет `git config user.*` и не подставляет чужой `--author`. В author,
  committer и трейлерах `*-by:` (`Co-authored-by:`, `Assisted-by:` …) нет AI (Claude, Codex,
  Copilot, Cursor, Devin, Gemini, Aider, Kimi …); в сообщении нет подписей «Generated with <AI>».
  `Co-authored-by:` людей разрешён. Проверка — `tools/check-authorship.sh <A..B>`, в CI — job
  `commits`.

## Безопасность станций

- **Root adb не ломать.** Не трогать adbd, маркеры `/data/quasar/activate_*` и
  `/data/quasar/rootkeeper.sh` (принадлежит root-методу): установщик только убирает из него
  строки-хуки, которые добавляли его старые версии, и никогда не создаёт, не удаляет и иначе не
  меняет файл.
- **Rootfs (`/system`, `/etc`) не менять**, всё постоянное живёт в `/data`. Единственное исключение
  — `/etc/init.d/S99zpulsar`: его пишет установщик и только после проверки, что rootfs не защищена
  (`rootfsGuard` в `installer/autostart.go`; [docs/development.md, «Установка и
  станция»](docs/development.md#установка-и-станция)).
- Защита станции от OTA прошивки Яндекса — не задача pulsar (владелец станции / root-метод). OTA,
  заменившая system-раздел, сносит `S99zpulsar`; повторный запуск установщика его восстанавливает,
  если проверка rootfs пропускает запись.
- Несколько устройств могут делить один adb-сервер: всегда `adb -s <serial>`.
- `adb shell` на этих прошивках — pty: в выводе `\r`, канал не binary-safe (бинарные данные — только
  `adb push`/`pull`), код выхода всегда 0 — подробности в
  [docs/development.md, «Правила installer/»](docs/development.md#правила-installer).
- Перед заменой бинаря сохранить рабочий как `.prev` — на нём держится автоматический откат. Ручной
  выход: `.prev` или `ctl.sh disable` (запускает стек Яндекса).
- Не открывать ESPHome API к станции, которую использует Home Assistant: тест — на отдельном бинаре
  на другом порту при остановленном сервисе (`ctl.sh disable` или остановка супервизора).
- Без согласия владельца — никаких изменений громкости и тестовых звуков на живой колонке; тесты со
  звуком ограничивают громкость: ≤ 0.2 на шкале `Mixer`, raw ≤ 82; выше — только с явного
  разрешения владельца.
- `/tmp` — небольшой tmpfs (размеры — [docs/hardware.md, «RAM и
  tmpfs»](docs/hardware.md#ram-и-tmpfs)): тестовые файлы удалять.
- Живые тесты — только по [docs/stations.md](docs/stations.md) (lock, тестовый экземпляр, очистка),
  процедура — skill `station-test`.
