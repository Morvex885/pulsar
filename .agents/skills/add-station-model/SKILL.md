---
name: add-station-model
description: >-
  Добавление новой модели станции Яндекса в pulsar: read-only сбор данных с неизвестной станции, предварительная Model в pulsar-core, измерения, установщик, boot.sh, docs/hardware.md. Ключевые слова: new model, add station model, devicetree codename, hackalice wiki, alsa_abi_probe, frontend_probe, ww_offline, hw_probe, experimental profile.
---

# Новая модель станции

Контекст: `docs/hardware.md` («Модели», «Заметки по моделям», «Источники»), `docs/architecture.md`,
`docs/development.md`, «Установка и станция». Безопасность — `AGENTS.md`, «Безопасность станций».

## 0. Новая модель неизвестна инструментам

`tools/station.sh` принимает только `midi|lite2|mini`, а установщик на неизвестной модели ничего не
ставит (`abis()` в `installer/probe.go` возвращает пустой список). Поэтому:

- Сбор и замеры — вручную: `adb -s <serial>` (всегда с серийником) или ssh на станцию.
- Только чтение; файлы — только в `/tmp/pulsar-test/`, после работы удалить.
- Стек Яндекса останавливается `/system/vendor/quasar/q stop` (возврат — `q start`): только с
  согласия владельца и с обязательным возвратом. `ctl.sh disable` не подходит: он, наоборот, запускает
  стек Яндекса, а на неизвестной модели pulsar не установлен.
- После добавления модели — добавить её цель в `tools/station.sh`, перечни целей в skill
  `station-test` и пункт «Громкость» в `docs/stations.md`; дальше работать по skill `station-test`.

## 1. Сбор данных (read-only)

1. Страница модели в вики: https://hackalice.me/devices .
2. На станции: `uname -a`, `cat /proc/device-tree/model`, `cat /proc/device-tree/compatible`,
   `cat /proc/device-tree/amlogic-dt-id`, `cat /proc/mtd`, `cat /proc/cpuinfo`,
   `cat /proc/meminfo`, `df /tmp`, `ls /sys/bus/i2c/devices`,
   `cat /system/vendor/quasar/VERSION`, `deviceType` из `/system/vendor/quasar/quasar.cfg`.
3. Userland: загрузчики `ls /lib/ld-linux-*`, версия glibc, `ls /etc/init.d`, `aplay -l`,
   `arecord -l`, `/etc/asound.conf` (в том числе `ipc_key` dmix), пресет VQE, каталог
   `spotter_models/`.
4. Периферия: `/sys/class/leds`, `/dev/fb*`, `/proc/bus/input/devices`, датчик освещённости,
   earcons.
5. Без `Model` из утилит с железом работает только `alsa_abi_probe` (ABI alsa-lib, без звука):
   `frontend_probe`, `ww_offline`, `hw_probe`, `player_probe`, `sendspin_probe` и `zigbee_probe` на
   неизвестной модели сразу выходят. Сетевые `lan_probe`, `update_probe`, `api_selftest` модель не
   определяют и работают. Сырая запись захвата:

   ```sh
   mkdir -p /tmp/pulsar-test && arecord -D <pcm> -f <формат> -c <каналы> -r <частота> \
     -d <секунды> /tmp/pulsar-test/capture.wav
   ```

   `<формат>` — S16_LE или S32_LE; `<pcm>`, формат, каналы и частота — из
   `arecord -D <pcm> --dump-hw-params -d 1 /dev/null`, `/etc/asound.conf` и строки «Захват» таблицы
   `docs/hardware.md`, «Модели». `ww_offline` читает только S16_LE; запись S32_LE (как у Мини) до
   `Model` можно только сохранить.

## 2. Код (места должны совпадать)

Сначала предварительная `Model` по данным п. 1, затем уточнение замерами (п. 3).

- `satellite/crates/pulsar-core/src/model.rs`: `static <NAME>: Model`, ветка в
  `model_from_compatible`; `detect_model` читает devicetree.
- Тест модели в `model.rs` по образцу `bergamot_is_mini3`; ориентиры кривой громкости — в
  `satellite/crates/pulsar-media/tests/mixer_golden.rs` (по образцу `mini3_curve_landmarks`).
- `installer/probe.go`: `modelName()`, `abis()`, `archOf()`, `stationCode()`; тесты — в
  `installer/probe_test.go`. Когда допустима вторая ABI в `abis()` — `docs/development.md`,
  «Установка и станция».
- Особые файлы установщика (как `asound.conf` для `yandexmini` в `installer/deploy.go`) — если
  модели нужен свой.
- `satellite/deploy/boot.sh`: список `yandex,(...)` в проверке `compatible`.
- Без `yandex,*` в `compatible` нужен маркер по образцу `mini_from_board_and_cfg` (`model.rs`) и
  проверки в `probe()` (`installer/probe.go`) и `boot.sh`.
- Правило «три места» (`model.rs`, `installer/probe.go`, `boot.sh`) — `docs/development.md`,
  «Установка и станция».
- Стоковый init-скрипт (`ls /etc/init.d`): `/etc/init.d/S99zpulsar` должен сортироваться после него
  (`installer/autostart.go`); запись о нём — в цепочке загрузки, `docs/development.md`, «Установка и
  станция».
- Ключ dmix: если в `/etc/asound.conf` станции свой `ipc_key`, добавить его в `clear_dmix`
  (`satellite/deploy/supervise.sh`) и `cleanup` (`tools/station.sh`); иначе зависший lock не
  снимется (`docs/hardware.md`, «Зависший dmix lock»).

## 3. Измерения

После п. 2 собрать утилиты: `make -C satellite ARCH=<arch> tools` →
`satellite/build/<arch>/tools/<name>` (`AGENTS.md`, «Сборка и проверки»). Запуск — вручную (п. 0),
при остановленном стеке; звук и яркость — только с согласия владельца:

- `frontend_probe --record` — микрофонный тракт, сырая запись;
- `ww_offline` — спотер на записи;
- `hw_probe` — светодиоды, часы, ввод, датчик.

Результаты вносят в `Model` и в документацию (п. 4).

## 4. Документация

- `docs/hardware.md`, «Модели»: колонка модели с источником (контрибьютор и PR) и ссылкой на вики.
- `docs/hardware.md`, «Источники»: пункт модели (контрибьютор, PR, вики).
- `docs/hardware.md`, «RAM и tmpfs»: строка модели.
- Экспериментальная модель — отдельный `docs/<model>.md` по образцу `docs/mini3.md`.
- `README.md`: таблица моделей в шапке, «Требования» (модель и прошивка), раздел «Модели:
  особенности», список имён устройств `yandex-<префикс>-xxxxxx` в «Установке» и остальные
  перечисления моделей (`grep -n '<соседняя модель>' README.md`) — добавить новую модель.
- `docs/development.md`, «Установка и станция» (цепочка загрузки): стоковый init-скрипт модели;
  `docs/hardware.md`, «Зависший dmix lock»: ключи IPC модели.
- Правила текста — `AGENTS.md`, «Документация».

## 5. Проверка

`make -C satellite check` и `go -C installer test ./...`; затем
`git grep -n -i -e '<соседняя модель>' -e '<codename соседа>'` по всему репозиторию — каждое найденное
место проверить (перечни моделей, цели, ключи); затем skill `pre-publish-review`.
