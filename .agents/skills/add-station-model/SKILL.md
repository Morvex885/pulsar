---
name: add-station-model
description: Добавление новой модели станции Яндекса в pulsar: read-only сбор данных с неизвестной станции, измерения, Model в pulsar-core, установщик, boot.sh, docs/hardware.md. Ключевые слова: new model, add station model, devicetree codename, hackalice wiki, alsa_abi_probe, frontend_probe, ww_offline, hw_probe, experimental profile.
---

# Новая модель станции

Контекст: `docs/hardware.md` («Модели», «Заметки по моделям», «Источники»), `docs/architecture.md`,
`docs/development.md`, «Установка и станция». Безопасность — `AGENTS.md`, «Безопасность станций».

## 0. Новая модель неизвестна инструментам

`tools/station.sh` принимает только `midi|lite2|mini`, а установщик на неизвестной модели ничего не
ставит (`abis()` в `installer/probe.go` возвращает пустой список). Поэтому до шага 3:

- Сбор и замеры — вручную: `adb -s <serial>` (всегда с серийником) или ssh на станцию.
- Только чтение; файлы — только в `/tmp/pulsar-test/`, после работы удалить.
- Остановка стека Яндекса (`ctl.sh disable` и подобное) — только с согласия владельца и с
  обязательным возвратом.
- После добавления модели — добавить её цель в `tools/station.sh` и дальше работать по skill
  `station-test`.

## 1. Сбор данных (read-only)

1. Страница модели в вики: https://hackalice.me/devices .
2. На станции: `uname -a`, `cat /proc/device-tree/model`, `cat /proc/device-tree/compatible`,
   `cat /proc/device-tree/amlogic-dt-id`, `cat /proc/mtd`, `cat /proc/cpuinfo`,
   `ls /sys/bus/i2c/devices`, `cat /system/vendor/quasar/VERSION`, `deviceType` из
   `/system/vendor/quasar/quasar.cfg`.
3. Userland: загрузчики `ls /lib/ld-linux-*`, версия glibc, `aplay -l`, `arecord -l`,
   `/etc/asound.conf`, пресет VQE, каталог `spotter_models/`.
4. Периферия: `/sys/class/leds`, `/dev/fb*`, `/proc/bus/input/devices`, датчик освещённости,
   earcons.

## 2. Измерения

Утилиты: `make -C satellite ARCH=<arch> tools` → `satellite/build/<arch>/tools/<name>`
(`AGENTS.md`, «Сборка и проверки»). Запуск — вручную (п. 0), после остановки демона; звук и
яркость — только с согласия владельца:

- `alsa_abi_probe` — ABI alsa-lib, без звука;
- `frontend_probe --record` — микрофонный тракт, сырая запись;
- `ww_offline` — споттер на записи;
- `hw_probe` — светодиоды, часы, ввод, датчик.

## 3. Код (места должны совпадать)

- `satellite/crates/pulsar-core/src/model.rs`: `static <NAME>: Model`, ветка в
  `model_from_compatible`; `detect_model` читает devicetree.
- Тест модели в `model.rs` по образцу `bergamot_is_mini3`; ориентиры кривой громкости — в
  `satellite/crates/pulsar-media/tests/mixer_golden.rs` (по образцу `mini3_curve_landmarks`).
- `installer/probe.go`: `modelName()`, `abis()`, `archOf()`, `stationCode()`; тесты — в
  `installer/probe_test.go`. Второй ABI — только для userland, реально виденного на этой модели.
- Особые файлы установщика (как `asound.conf` для `yandexmini` в `installer/deploy.go`) — если
  модели нужен свой.
- `satellite/deploy/boot.sh`: список `yandex,(...)` в проверке `compatible`.
- Без `yandex,*` в `compatible` нужен маркер по образцу `mini_from_board_and_cfg` (`model.rs`) и
  проверки в `probe()` (`installer/probe.go`) и `boot.sh`.
- Правило «три места» (`model.rs`, `installer/probe.go`, `boot.sh`) — `docs/development.md`,
  «Установка и станция».

## 4. Документация

- Колонка в таблице `docs/hardware.md`, «Модели»: с источником (контрибьютор и PR) и ссылкой на
  вики.
- Экспериментальная модель — отдельный `docs/<model>.md` по образцу `docs/mini3.md`.
- Список моделей в `README.md` — добавить новую модель.
- Правила текста — `AGENTS.md`, «Документация».

## 5. Проверка

`make -C satellite check` и `go -C installer test ./...`; затем skill `pre-publish-review`.
