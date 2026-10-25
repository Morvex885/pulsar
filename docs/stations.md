# Протокол станций

Правила для агентов и людей, которые тестируют pulsar на живых станциях. Помощник
`tools/station.sh` их реализует.

- **Адреса.** `PULSAR_MIDI` и `PULSAR_LITE2` в окружении — ssh-назначение (`root@<ip>` или алиас
  из `~/.ssh/config`). В репозиторий они не попадают. Если переменной нет — статус
  `заблокирован: нет PULSAR_<X>`.
- **Lock.** Всё, что меняет состояние станции, — только под своим lock. Команда:
  `PULSAR_TAG=<метка> tools/station.sh <midi|lite2> lock <минуты>`. Метка — произвольное слово без
  пробелов, по которому видно, чья это работа (ветка, задача).
  - Срок заявляется честно, 1–180 минут; живая подмена — минимум 30.
  - Каждая меняющая команда скрипта проверяет, что lock принадлежит `PULSAR_TAG`.
  - Чужой lock можно снять командой `break-lock` только после заявленного срока + 30 минут.
    Каталог lock без файла `owner` (оборванный захват) `break-lock` снимает сразу. `lock` пишет
    `owner` через временный файл и `mv`, поэтому файл либо целый, либо его нет.
  - Порядок завершения сессии: `test-stop` (если запускали) → `start` или `restore` → `cleanup` →
    `unlock`. `cleanup` отказывает, пока жив процесс из `/tmp/pulsar-test/pid`.
  - Свободное место проверяется `df -Pk` (busybox `df -k` переносит строку при длинном имени
    устройства).
- **Аппаратные тесты** (аудио, плеер, Sendspin-sink, LED, часы, ввод, датчик, радио) и **любые
  тесты ESPHome API** (`api_selftest`, тестовый экземпляр) — только при остановленном сервисе
  (требование AGENTS.md):
  - `stop` останавливает supervisor и демон в порядке `ctl.sh reload`, Yandex-стек не
    запускается;
  - если на станции Yandex-режим (нет `/data/pulsar/enabled`), `stop` отказывает. Тогда статус
    `заблокирован: станция в Yandex-режиме`, сообщить владельцу;
  - после теста — `start` (`ctl.sh reload`).
- **Сетевые тесты без API** (mDNS-проба, арбитраж, HTTPS) можно гонять при работающем демоне:
  - только на альтернативных портах (16054 и т. п.);
  - mDNS — только с сервисным типом `_pulsartest._tcp`, никогда не `_esphomelib._tcp` и не
    `_sendspin._tcp`;
  - к портам 6053, 6638, 8928 живого демона не подключаться. На 6638 deCONZ-сервер держит одного
    клиента и выбил бы Z2M/ZHA.
- **Тестовый экземпляр демона.** Последовательность: `push build/<arch>/pulsar` → `stop` →
  `test-start` → … → `test-stop` → `start`.
  - `test-start` собирает `/tmp/pulsar-test/state.conf` из `/data/pulsar/state.conf`: удаляет
    строки `volume=`, `volume2=`, `volume_curve=`, `muted=`, `auto_update=`, `update_attempt=` и
    дописывает `volume2=0.2`, `muted=0`, `auto_update=0`.
  - `api_key` не копируется (plaintext API), копии `zigbee-*.ybl` кладутся рядом.
  - Запуск: `PULSAR_PORT_OFFSET=10000` через `nohup`, лог в `/tmp/pulsar-test/log`.
  - Лог тестового экземпляра: `test-log [N]` (последние N строк `/tmp/pulsar-test/log`, по
    умолчанию 50); лог демона — `log [N]`.
  - Клиент — `tools/esphome_check.py --host <ip> --port 16053`.
  - Перед сценариями со звуком проверить `amixer -c0 cget name='Master Playback Volume'`:
    raw ≤ 82.
- **Убитый во время звука процесс.** Тест, который убивает играющий процесс
  (`kill -SEGV`/`kill -9` тестового экземпляра или probe во время звонка, TTS, музыки), оставляет
  устаревший dmix-lock прошивочной alsa-lib (заголовок `satellite/deploy/supervise.sh`): после
  него станция молчит до перезагрузки, перезапуск демона не помогает. После такого теста —
  обязательно очистка: `test-stop`, `stop` и `cleanup` снимают lock (печатают
  `removed stale ALSA dmix lock (…)`); `start` тоже: `supervise.sh` снимает lock перед каждым
  запуском демона. Вручную — `ipcrm -M 0x400 -M 0x401 -S 0x400`, только когда в `ipcs -m` у
  ключей 0x400/0x401 nattch 0.
- `push` и `swap` проверяют передачу: файл льётся в `*.part`, сверяются размер и sha256 (без
  `sha256sum` на станции — только размер, с предупреждением); при расхождении `.part` удаляется,
  код возврата 1.
- **Живая подмена**: `swap <бинарь>` → наблюдение за логом → обязательно `restore`.
  - `swap` требует `enabled` и ≥ 30 МБ свободного места в `/data`.
  - При первой подмене установленный бинарь сохраняется в `pulsar.orig`, а `.prev` владельца — в
    `pulsar.prev.orig`. На время подмены `.prev` = `pulsar.orig`, так что откат supervisor'а
    возвращает бинарь владельца.
  - `restore` возвращает бинарь и `.prev`, удаляет `pulsar.orig`.
  - Если supervisor вернул станцию Yandex-стеку (`enabled` удалён после краш-цикла), `restore`
    выполняет `ctl.sh enable`.
  - Midi в роли `pulsar-coordinator`: живая подмена — только при двух условиях:
    1. Read-only чтение `tools/deconz_read.py` тестового экземпляра (16638) совпадает с чтением
       `zigbee_probe` установленной версии (порт 16639, при остановленном сервисе): device state,
       MAC, PAN, ext PAN, канал и NWK update id.
    2. Владелец подтвердил свежий бэкап Z2M/ZHA.
    - Без подтверждения подмена на Midi не делается, пункт уходит в чек-лист владельца.
- **Громкость.** Любой тест со звуком — при громкости ≤ 0.2 по шкале Mixer (raw ≤ 82 в
  `Master Playback Volume`, `amixer -c0 cget/cset name='Master Playback Volume'`). Probe-утилиты
  выставляют громкость сами и возвращают исходную. Тестовый экземпляр получает её из
  подготовленного `state.conf`. Живой демон после `start` берёт громкость владельца из
  `/data/pulsar/state.conf`.
- **Радио Zigbee.**
  - Читать — только сырым скриптом `tools/deconz_read.py`: команды 0x0D, 0x07, 0x0A. Подключение
    вытесняет текущего deCONZ-клиента (Z2M/ZHA отключаются), поэтому скрипт запускается только
    против тестового экземпляра (16638) или порта `zigbee_probe` при остановленном сервисе,
    никогда против 6638 живого демона. zigpy, `ControllerApplication`, write parameter и смена
    network state запрещены.
  - Прошивать — только образ текущей роли из `state.conf`: `zigbee-coordinator.ybl` для
    `pulsar-coordinator`, `zigbee-router.ybl` для `router`. При роли `off` не прошивать.
- **`/tmp` — tmpfs** (Lite 2: 237 МБ RAM). Утилиты собираются уже стрипнутыми, в конце сессии —
  `cleanup`.

## station.sh

`tools/station.sh` (POSIX sh; на станции выполняется busybox ash) реализует эти правила: lock,
`stop`/`start`, `push`, тестовый экземпляр, живая подмена, очистка dmix-lock. Вызов:
`PULSAR_TAG=<метка> tools/station.sh <midi|lite2> <команда>`. Адреса станций берутся из
`PULSAR_MIDI`/`PULSAR_LITE2` и в репозиторий не попадают. Python-утилиты в `tools/`
(`esphome_check.py`, `sendspin_server.py`, `deconz_read.py`) ставят зависимости из
`tools/requirements.txt`.
