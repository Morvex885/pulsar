---
name: add-station-model
description: >-
  Добавление новой модели станции Яндекса в pulsar: read-only сбор данных с неизвестной станции, предварительная Model в pulsar-core, измерения, установщик, boot.sh, docs/hardware.md и docs/<модель>.md. Ключевые слова: new model, add station model, devicetree codename, hackalice wiki, alsa_abi_probe, frontend_probe, ww_offline, hw_probe, experimental profile.
---

# Новая модель станции

Контекст: `docs/hardware.md` («Модели»: вики и docs моделей), docs моделей `docs/<модель>.md`,
`docs/architecture.md`, `docs/development.md`, «Установка и станция». Безопасность — `AGENTS.md`,
«Безопасность станций».

## 0. Новая модель неизвестна инструментам

`tools/station.sh` принимает только `midi|lite2|mini`, а установщик на неизвестной модели ничего не
ставит (`abis()` в `installer/probe.go` возвращает пустой список). Поэтому:

- Сбор и замеры — вручную: `adb -s <serial>` (всегда с серийником) или ssh на станцию.
- Только чтение; файлы — только в `/tmp/pulsar-test/`, после работы удалить.
- Стек Яндекса останавливается `/system/vendor/quasar/q stop` (возврат — `q start`): только с
  согласия владельца и с обязательным возвратом. `ctl.sh disable` не подходит: он, наоборот, запускает
  стек Яндекса, а на неизвестной модели pulsar не установлен.
- После добавления модели — добавить её цель по skill `station-test`, «Допуск», и дальше работать по
  нему.

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
   `arecord -D <pcm> --dump-hw-params -d 1 /dev/null`, `/etc/asound.conf` и раздела «Звук» doc
   соседней модели (`docs/<модель>.md`). `ww_offline` читает только S16_LE; запись S32_LE (как у
   Мини) до `Model` можно только сохранить.

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
- Ключ dmix: если `ipc_key` dmix воспроизведения модели (в `/etc/asound.conf` станции или в своём
  asound.conf установщика) не 1024 и не 2277 — добавить его в `clear_dmix`
  (`satellite/deploy/supervise.sh`) и `cleanup` (`tools/station.sh`); иначе зависший lock не
  снимется (`docs/hardware.md`, «Зависший dmix lock»).

## 3. Измерения

После п. 2 собрать утилиты: `make -C satellite ARCH=<arch> tools` →
`satellite/build/<arch>/tools/<name>` (`AGENTS.md`, «Сборка и проверки»). Запуск — вручную (п. 0),
при остановленном стеке; звук и яркость — только с согласия владельца:

- `frontend_probe --record` — микрофонный тракт, сырая запись;
- `ww_offline` — спотер на записи;
- раскладка спотеров — по строке `channels [...]` у `ww_offline`: каждому флагу каналов из
  `flags.txt` модели (`bnr-channel`, `raw-mic-channels`, `freq-filter-audio-channel raw-mic`)
  соответствует выход из таблицы `docs/hardware.md`, «Библиотеки Яндекса (C API)»;
  `out N: none in the preset` — в пресете VQE модели нет выхода из таблицы (демон тогда пишет
  `legacy channels`);
- `hw_probe` — светодиоды, часы, ввод, датчик.

Результаты вносят в `Model` и в документацию (п. 4).

## 4. Документация

- `docs/<модель>.md` — новый doc модели по шаблону ниже; имя файла — короткий идентификатор модели,
  как у соседей (`midi`, `lite2`, `mini2`, `mini3`, `mini`).
- `docs/hardware.md`, «Модели»: строка указателя — модель, кодовое имя, данные (станция
  мейнтейнера или контрибьютор и PR), ссылка на страницу вики, ссылка на doc модели.
- `docs/hardware.md`, «RAM и tmpfs»: строка модели (МиБ, MemTotal / 1024; источник — вики или
  замер).

Шаблон doc модели (образцы — `docs/mini3.md`, `docs/mini2.md`):

```markdown
# <Модель> (`<codename>`)

<Источник данных: «Станция мейнтейнера, данные авторитетны» или «Данные контрибьютора, не
перепроверены: PR [#N](https://github.com/Morvex885/pulsar/pull/N) (<ник>)»>. Железо —
[вики](https://hackalice.me/devices/<страница>/); здесь только то, чего там нет
([AGENTS.md, «Документация»](../AGENTS.md#документация)).

<Если обследовано несколько станций: прошивки, совпадение SHA256 библиотек и конфигов.>

## Userland и хранилище
## Звук
## Светодиоды и часы
## Кнопки и микрофоны
## Проверено
## Не проверено
```

- Разделы — в этом порядке и с этими именами (на них ссылаются код и docs); раздел без фактов не
  создавать, кроме «Не проверено» у модели контрибьютора.
- Только то, чего нет на странице вики: замеры (раскладка каналов захвата, чанк VQE,
  кривая громкости), особенности, на которых стоит код, расхождения с вики с источником, статус
  проверок. Факты, которые на вики есть, не переписывать (`AGENTS.md`, «Документация»).
- «Звук»: захват (PCM, каналы, частота, формат, какой канал — микрофон и референс), воспроизведение,
  громкость, VQE (пресет, микрофоны/референсы, чанк), спотеры, alsa-lib (версия, версионированная
  или нет).
- «Проверено» — что именно, на какой прошивке и кем; «Не проверено» — всё из `Model`, что не
  прошло через железо (кольцо, часы, кнопки, mute, установка, автозапуск).
- Комментарий над `static <NAME>: Model` в `model.rs` — одна строка-ссылка на doc модели, без
  пересказа фактов.

Остальное:

- `README.md`: таблица моделей в шапке, «Требования» (модель и прошивка), раздел «Модели:
  особенности», список имён устройств `yandex-<префикс>-xxxxxx` в «Установке» и остальные
  перечисления моделей (`grep -n '<соседняя модель>' README.md`) — добавить новую модель.
- `docs/development.md`, «Установка и станция» (цепочка загрузки): стоковый init-скрипт модели;
  `docs/hardware.md`, «Зависший dmix lock»: ключи IPC модели.
- Правила текста — `AGENTS.md`, «Документация».

## 5. Проверка

`make -C satellite check` и `go -C installer test ./...`. Затем найти остальные места по всему
репозиторию и проверить каждое (перечни моделей, цели, ключи):

```sh
git grep -n -i -e '<соседняя модель>' -e '<она же по-английски>' -e '<идентификатор, напр. lite2>' -e '<codename>'
```

Затем skill `pre-publish-review`.
