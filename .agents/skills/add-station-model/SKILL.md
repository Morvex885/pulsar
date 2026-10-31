---
name: add-station-model
description: Добавление новой модели станции Яндекс в pulsar: read-only сбор данных, измерения, Model в pulsar-core, installer/probe.go, boot.sh, docs/hardware.md. Use for new model, add station model, device tree codename, hackalice wiki, alsa_abi_probe, frontend_probe, ww_offline, hw_probe, experimental profile.
---

# Новая модель станции

Контекст: `docs/hardware.md` («Модели», «Заметки по моделям», «Источники»), `docs/architecture.md`.
Любое обращение к живой станции — по skill `station-test` (lock, `stop`, запреты).

## 1. Сбор данных (read-only)

1. Страница модели в вики: https://hackalice.me/devices .
2. На станции, только чтение: `uname -a`, `cat /proc/device-tree/model`,
   `cat /proc/device-tree/compatible`, `cat /proc/device-tree/amlogic-dt-id`, `cat /proc/mtd`,
   `cat /proc/cpuinfo`, `ls /sys/bus/i2c/devices`, `cat /system/vendor/quasar/VERSION`,
   `deviceType` из `/system/vendor/quasar/quasar.cfg`.
3. Userland: загрузчики `ls /lib/ld-linux-*`, версия glibc, `aplay -l`, `arecord -l`,
   `/etc/asound.conf`, пресет VQE, каталог `spotter_models/`.
4. Периферия: `/sys/class/leds`, `/dev/fb*`, `/proc/bus/input/devices`, датчик освещённости,
   earcons.

Ничего не записывать на станцию, кроме `/tmp/pulsar-test/` через `station.sh push`.

## 2. Измерения

По skill `station-test`, под lock и после `stop`; утилиты собираются `make ARCH=<arch> tools`
(`docs/architecture.md`, «Workspace»):

- `alsa_abi_probe` (ABI alsa-lib, без звука);
- `frontend_probe --record` (микрофонный тракт, сырая запись);
- `ww_offline` (споттер на записи);
- `hw_probe` (светодиоды, часы, ввод, датчик). Звук и яркость — только с согласия владельца.

## 3. Код (три места должны совпадать)

- `satellite/crates/pulsar-core/src/model.rs`: новый `static <NAME>: Model`, ветка в
  `model_from_compatible`; `detect_model` читает devicetree.
- `installer/probe.go`: `modelName()` и `abis()` (по `abis()` `stationCode()` узнаёт модель, а
  `archOf()` выбирает сборку); особый маркер (как у Мини) — в `probe()`. Второй ABI — только для
  userland, реально виденного на этой модели.
- `satellite/deploy/boot.sh`: список `yandex,(...)`; особый маркер (как у Мини) — только если в
  `compatible` нет `yandex,*`.
- Расхождение между тремя местами = станция не установится или не запустится.

## 4. Документация

- Колонка в таблице `docs/hardware.md`, «Модели»: с источником (контрибьютор и PR) и ссылкой на
  вики.
- Экспериментальная модель — отдельный `docs/<model>.md` по образцу `docs/mini3.md`.
- Список моделей в `README.md` — добавить новую модель.
- Правила текста — `AGENTS.md`, «Документация».

## 5. Проверка

`make -C satellite check` и `go -C installer test ./...`; затем skill `pre-publish-review`.
