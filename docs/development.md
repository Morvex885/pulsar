# Разработка

Справочник по сборке, CI и релизам, установке на станцию, супервизору, обновлениям из HA, правилам
`installer/` и crash-отчёту: пины, контракты между компонентами и версиями, внешнее поведение. Чего
здесь нет: команд проверки и правил репозитория (AGENTS.md), протокола работы на живых станциях
([stations.md](stations.md)), фактов железа (вики и docs моделей, указатель —
[hardware.md, «Модели»](hardware.md#модели)); поведение кода — в коде, обоснования — в
комментариях у кода.

## Сборка

Сборка — на Linux или WSL2 (кросс-компиляция через cargo-zigbuild); сборка демона на macOS не
поддерживается. Версии инструментов совпадают с CI (`.github/workflows/build.yml`):

- [rustup](https://rustup.rs/) — сам ставит нужную версию Rust и обе цели из
  `satellite/rust-toolchain.toml`;
- cargo-zigbuild 0.24.0: `cargo install cargo-zigbuild --locked --version 0.24.0`;
- [zig 0.14.1](https://ziglang.org/download/) — компоновщик и C/C++-компилятор для станций; другой
  zig — `make -C satellite ZIG=/путь/к/zig`; сборки под станции отклоняют zig 0.11–0.13: их
  armhf-сборки C++ в release (sendspin-cpp) падают до `main`. Цель armhf — glibc 2.23 (userland
  Мини), цель aarch64 — glibc 2.36 (обычный userland Миди); glibc станций —
  вики и docs моделей ([hardware.md, «Модели»](hardware.md#модели));
- `make`, `git`, `curl`, `patch`, компилятор C для хоста (`cc`); для sendspin-cpp
  ([«Линковка C++ (sendspin-cpp)»](architecture.md#линковка-c-sendspin-cpp)) — `cmake` ≥ 3.16 и
  `ninja` (`uv tool install cmake`, `uv tool install ninja`);
- [Go](https://go.dev/dl/) 1.27+ (`installer/go.mod`) — для установщика из копии репозитория;
- `adb` из Android platform-tools.

- **`make check` берёт zig любой версии**: clippy под станции только проверяет типы, а из C
  компилирует лишь `ring` в build-скриптах. cargo-zigbuild и C-компилятор хоста (`cc`) ему тоже
  нужны.

Выходы `make -C satellite` на каждую архитектуру, `satellite/build/<arch>/`:

- `pulsar` — стрипнутый, именно он ставится на станцию;
- `pulsar.debug` — нестрипнутый, с таблицами строк, для разбора crash-отчёта (см.
  [«Диагностика»](#диагностика)).

`ssh/build.sh` собирает dropbear и sftp-server для `pulsar-install --ssh` и архивов релиза.

Версия бинарника — `git describe --tags --match 'v*' --always --dirty`
([`../satellite/Makefile`](../satellite/Makefile), `VERSION`). Учитываются только релизные теги
`v*`: апдейтер разбирает версию как `vX.Y.Z[-N-g<hash>]` и сверяет `--version` нового бинарника с
тегом релиза.

## CI и релизы

Push тега `v*` публикует релиз (job `release` в `.github/workflows/build.yml`). Имена ассетов —
постоянный контракт; от них зависят:

| Ассет | Кто зависит |
|---|---|
| `pulsar-windows-amd64.zip`, `pulsar-macos-{arm64,amd64}.tar.gz`, `pulsar-linux-{amd64,arm64}.tar.gz` | ссылки README `releases/latest/download/…`; внутри — раскладка репозитория (собирает [`../satellite/deploy/dist.sh`](../satellite/deploy/dist.sh)), по которой `pulsar-install` находит бинарники, образы радио и скрипты рядом с собой |
| `pulsar-<arch>.bin` (`aarch64`, `armhf`) | апдейтер: `releases/download/<tag>/pulsar-<arch>.bin` |
| `zigbee-coordinator.ybl`, `zigbee-router.ybl` | апдейтер (только Миди; релиз без образа обновляется без него); архивы — `radio/out/zigbee-*.ybl` для установщика. Сами образы — [zigbee.md, «Образы .ybl и бутлоадер»](zigbee.md#образы-ybl-и-бутлоадер) |
| `SHA256SUMS` | апдейтер сверяет по нему `.bin` и `.ybl` |
| `pulsar-<arch>.debug` | разбор crash-отчёта бинарника из релиза |

Тег должен совпадать с `git describe` бинарника: job `build` собирает из чекаута тега с полной
историей (`fetch-depth: 0`), поэтому совпадает. Пользователям предлагаются только стабильные релизы
(не черновики и не пре-релизы) — как их находит апдейтер, см.
[«Обновления из HA»](#обновления-из-ha).

## Установка и станция

```sh
go -C installer run . --list         # найти станции и определить модель, ничего не ставить
go -C installer run . <ip>[:port]    # установить или обновить
```

В чекауте установщик сначала сам вызывает `make` (с `--ssh` — и `ssh/build.sh`). Флаги — usage в
[`../installer/main.go`](../installer/main.go).

**Детект модели — три места, которые должны совпадать:** демон (`detect_model()` в
[`../satellite/crates/pulsar-core/src/model.rs`](../satellite/crates/pulsar-core/src/model.rs)),
установщик (`modelName()` / `abis()` / `stationCode()` и особый маркер Мини в `probe()` в
[`../installer/probe.go`](../installer/probe.go)) и
[`../satellite/deploy/boot.sh`](../satellite/deploy/boot.sh) (список кодовых имён `yandex,<codename>`
из `/proc/device-tree/compatible`). У Мини записи `yandex,*` нет, и все три места требуют ОБА
признака: `/proc/device-tree/amlogic-dt-id` = `axg_s420_v03` и `deviceType=yandexmini` в
`/system/vendor/quasar/quasar.cfg` (почему одного id мало —
[mini.md, «Userland и хранилище»](mini.md#userland-и-хранилище)).
Новая модель добавляется во все три места.

**Выбор сборки** (aarch64 или armhf) — комментарий у `abis()` в
[`../installer/probe.go`](../installer/probe.go). Правило: вторая ABI в `abis()` — только для
userspace, действительно встреченного на этой модели.

**Раскладка `/data/pulsar`:** `pulsar`, `pulsar.prev`, `pulsar.bad`, `boot.sh`, `supervise.sh`,
`ctl.sh`, `state.conf`, `api_key`, `enabled`, `sendspin/`, `zigbee-coordinator.ybl`,
`zigbee-router.ybl` (Миди), `asound.conf` (только Мини).

- `.prev` — цель отката супервизора; `.bad` — бинарник, заменённый откатом после crash-цикла.
- `state.conf` — формат и совместимость с `.prev`: [architecture.md, «state.conf и откат на
  .prev»](architecture.md#stateconf-и-откат-на-prev).
- `api_key` — Noise PSK ESPHome: base64 от 32 байт, режим 0600. Пишут автонастройка HA (по API) или
  `ctl.sh api-key`. Нет файла — API без шифрования; повреждённый файл — сервер работает, но не
  пускает ни одного клиента (fail closed, без отката на plaintext); детали —
  [`../satellite/crates/pulsar-esphome/src/key.rs`](../satellite/crates/pulsar-esphome/src/key.rs).
- `enabled` — пока файл есть, `boot.sh` и супервизор держат pulsar вместо стека Яндекса.
- `asound.conf` (Мини) пишет только установщик. Обновление из HA меняет лишь бинарник и образы
  радио, поэтому правка `satellite/deploy/asound.conf` доходит до станции только при повторном
  запуске `pulsar-install`.
- Вне `/data` — только `/etc/init.d/S99zpulsar` (автозапуск; пишет установщик, правила —
  AGENTS.md, «Безопасность станций»).
- Лог: `/tmp/pulsar.log` (`ctl.sh log N`; ротация — заголовок `supervise.sh`). Путь — общий
  контракт `supervise.sh`, `ctl.sh`, установщика (health check) и `tools/station.sh`. `/tmp` —
  tmpfs: [hardware.md, «RAM и tmpfs»](hardware.md#ram-и-tmpfs).

**Цепочка загрузки:** busybox init → `rcS` (запускает `/etc/init.d/S??*` по порядку с `start` и
ждёт каждый) → скрипт прошивки: `S97quasar` (Миди), `S99quasar` (Лайт 2; Мини 2 — по данным
контрибьюторов), `S99onboot.sh` (Мини, по данным контрибьюторов) → `onboot.sh` (adbd через
`activate_adb.sh check`, quasar в фоне; возвращается) → `S99zpulsar` → `boot.sh` в фоне →
ожидание quasar, `/system/vendor/quasar/q stop` (`q` есть на Лайт 2; Мини 2 — по данным
контрибьюторов) → `supervise.sh`. Мини 3 (`bergamot`) и Мини 3 Про (`pickle`) поддерживают
`boot.sh` и установщик, но их скрипты прошивки не записаны. Условия, при которых `boot.sh` не
трогает стек Яндекса, — в его заголовке и комментариях. Root через AliceBridge добавляет в
`/etc/init.d` `S99zalicebridge-ssh` и `S99zzalicebridge-agent` (файлы метода root): `S99zpulsar`
идёт между ними. По [AGENT.md](https://github.com/Yandex-Station-Universal-Repo/AliceBridge_APP/blob/main/AGENT.md)
агент восстанавливает сеть, ADB/SSH и защиту от OTA и обновляет себя; стека Яндекса в его задачах
нет.

**Ручная замена бинарника** (ssh — после `pulsar-install --ssh`; через adb — `adb push`):

```sh
scp satellite/build/<arch>/pulsar root@<ip>:/data/pulsar/pulsar.new
ssh root@<ip> 'cd /data/pulsar && cp pulsar pulsar.prev &&
  mv pulsar.new pulsar && ./ctl.sh restart'
```

## Супервизор

Контракт — заголовок [`../satellite/deploy/supervise.sh`](../satellite/deploy/supervise.sh)
(коды выхода, короткие сбои, откат на `.prev`, возврат стека Яндекса, ротация лога, очистка
зависшего dmix lock); здесь он не пересказывается. Механизм зависшего dmix lock —
[hardware.md, «Зависший dmix lock»](hardware.md#зависший-dmix-lock).

Контракт строк лога (`supervise.sh`, crash-отчёт демона, маркер установщика) с health check
установщика (`healthCheck()` в [`../installer/deploy.go`](../installer/deploy.go)): после маркера
установки в логе есть `=== start`, и начиная с первого из них — ровно одна такая строка (новый
демон), процесс `pulsar` жив, а сбоем считаются `=== CRASH` (пишет демон), `=== exit` и
`=== supervise`. Строка `=== alsa: removed stale dmix lock (…)`, которую `supervise.sh` пишет перед
запуском, если что-то удалил, — штатная очистка и сбоем считаться не должна.

## Обновления из HA

`pulsar-update`, сущности «Firmware» и «Auto update»; поведение для пользователя — README,
[«Обновление из Home Assistant»](../README.md#обновление-из-home-assistant). Что нельзя сломать:

1. **Имена ассетов** — постоянны, см. [«CI и релизы»](#ci-и-релизы).
2. **Поиск последнего релиза** — через редирект `https://github.com/<repo>/releases/latest` на
   `/releases/tag/<tag>` (GitHub отдаёт так новейший стабильный релиз), а не через REST API: его
   лимит 60 запросов в час на публичный IP исчерпывается станциями за общим выходом VPN / CGNAT.
3. **Маркер `update_attempt=<tag>` в `state.conf`** записывается на диск до подмены бинарника и
   стирается новым бинарником, если его версия равна `<tag>`. Маркер, оставшийся под другой
   версией, значит, что релиз не встал: откат `supervise.sh` или фатальная ошибка установки
   (`fatal` в `UpdateEvent::Installed`, `pulsar-update/src/updater.rs`; после нефатальной маркер
   стирается). Автообновление этот тег больше не ставит (вручную — можно). Ключ читают и пишут
   разные версии демона.

## Правила installer/

- `installer/` (Go) — единственный установщик на стороне хоста.
- Только стандартная библиотека Go и `golang.org/x/net`; из инструментов хоста — только adb (плюс
  `make` и `ssh/build.sh` в чекауте).
- Команды на станции — строки busybox sh через `sh()` в [`../installer/adb.go`](../installer/adb.go)
  (adb shell + маркер `__rc=`); одна команда — не больше 4 КБ на этих прошивках.
- `adb shell` на прошивках станций — pty: в выводе `\r`, канал не binary-safe (бинарные данные —
  только `adb push` / `adb pull`), код выхода всегда 0 (настоящий передаётся маркером `__rc=`).
- Чистые части (текст init-скрипта, проверка защищённости корневой ФС, чистка `rootkeeper.sh`)
  покрыты `autostart_test.go` и `rootkeeper_test.go`; они должны оставаться чистыми.

## Диагностика

При падении демон пишет в лог (`/tmp/pulsar.log`) отчёт, начинающийся с `=== CRASH`. Контракт:
бинарник собран non-PIE (`-C relocation-model=static`), поэтому адреса в отчёте абсолютные и
разбираются `addr2line -f -e` только по `pulsar.debug` той же сборки
(`satellite/build/<arch>/pulsar.debug`; для релиза — ассет `pulsar-<arch>.debug`). Формат отчёта —
[`../satellite/crates/pulsar/src/crash.rs`](../satellite/crates/pulsar/src/crash.rs) (заголовок и
запись полей в обработчике); процедура разбора — skill `crash-decode`.
