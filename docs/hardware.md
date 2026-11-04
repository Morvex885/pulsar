# Железо станций

Факты железа — на вики hackalice.me; страницы и docs моделей перечислены в [«Модели»](#модели),
правило отбора — в [AGENTS.md, «Документация»](../AGENTS.md#документация). Здесь общее для всех
моделей: C API библиотек Яндекса, ABI alsa-lib, RAM и tmpfs, зависший dmix lock. Детект модели,
выбор ABI, цепочка загрузки и раскладка `/data/pulsar` описаны в
[development.md, «Установка и станция»](development.md#установка-и-станция).

## Модели

| Модель | Кодовое имя | Данные | Вики | Doc модели |
|---|---|---|---|---|
| Миди | `cucumber` | станция мейнтейнера | [station-midi](https://hackalice.me/devices/station-midi/) | [midi.md](midi.md) |
| Лайт 2 | `plum`, `prune` | `plum` — станция мейнтейнера; `prune` — контрибьютор: [#57](https://github.com/Morvex885/pulsar/issues/57) | [station-lite2](https://hackalice.me/devices/station-lite2/) | [lite2.md](lite2.md) |
| Станция 2 | `station-midi` | контрибьютор: [#58](https://github.com/Morvex885/pulsar/pull/58) | [station-2](https://hackalice.me/devices/station-2/) | [station2.md](station2.md) |
| Мини 2 | `station-mini2` | контрибьюторы: [#9](https://github.com/Morvex885/pulsar/pull/9), станция из [#38](https://github.com/Morvex885/pulsar/issues/38) | [station-mini2](https://hackalice.me/devices/station-mini2/) | [mini2.md](mini2.md) |
| Мини 3 | `bergamot` | контрибьютор: [#30](https://github.com/Morvex885/pulsar/pull/30) | [station-mini3](https://hackalice.me/devices/station-mini3/) | [mini3.md](mini3.md) |
| Мини 3 Про | `pickle` | контрибьютор: [#52](https://github.com/Morvex885/pulsar/pull/52) | [station-mini3-pro](https://hackalice.me/devices/station-mini3-pro/) | [mini3pro.md](mini3pro.md) |
| Мини | `yandexmini` | контрибьюторы: [#35](https://github.com/Morvex885/pulsar/pull/35), [#68](https://github.com/Morvex885/pulsar/pull/68), [#74](https://github.com/Morvex885/pulsar/issues/74) | [station-mini](https://hackalice.me/devices/station-mini/) | [mini.md](mini.md) |

Авторитетны данные Миди и Лайт 2 на плате `plum`; данные контрибьюторов не перепроверены.

## Библиотеки Яндекса (C API)

Wake word, командный спотер и эхоподавление работают на проприетарных библиотеках прошивки.
Демон загружает их через `dlopen` во время работы, в репозиторий они не копируются
([AGENTS.md](../AGENTS.md#git), «Git»). C API восстановлен реверсом `maind`, а сигнатуры, которые
использует код, лежат в
[`../satellite/crates/pulsar-audio/src/engine/`](../satellite/crates/pulsar-audio/src/engine/).

`libyandex_evil.so` лежит в `/system/vendor/quasar/libs/` вместе с `libtflite.so` и
`libiconv.so`.

- VQE: `YandexVqeC_Config_CreateFromPreset(preset, &cfg)`,
  `YandexVqeC_Create(cfg, models_dir, 0, nullptr, &vqe)`,
  `YandexVqeC_Process(vqe, mics, nmic * chunk, spk, nspk * chunk)`. Сэмплы передаются как float в
  [-1, 1], с чередованием по кадрам. Размер входного чанка (`GetInputChunkSize`) и выходного кадра
  (`GetProcessingFrame`) задаёт пресет; значения по моделям — в разделе «Звук» docs моделей
  ([«Модели»](#модели)).
- `YandexVqeC_GetOutputChannelData(vqe, type, idx, &ptr, &n)` всегда возвращает «нет ошибки».
  Если выхода такого типа нет, функция отдаёт `n == 0`.
- `YandexVqeC_Config_GetOutputChannelCount` принимает `(cfg, type, int *out)`. Остальные геттеры
  `Config_Get*` возвращают значение напрямую.
- Спотер: `spotter_create_multichannel(conf, rate, n, types, false, &err)` и
  `spotter_submit_raw_data(sp, s16_interleaved, bytes, &err)`. Сколько каналов нужно каждому
  типу, задаёт конфиг модели спотера (`spotter_conf_get_required_channel_count`). Какой выход VQE
  питает каждый тип канала — ниже.
- Каналы спотера → выходы VQE (`voicetech.AudioInputChannelType`, имена
  `AUDIO_INPUT_CHANNEL_TYPE_*` в `libyandex_evil.so`) — контракт библиотеки, общий для всех
  моделей. Таблица восстановлена реверсом конвертеров `maind` (`toSpotterChannelType`,
  [#60](https://github.com/Morvex885/pulsar/pull/60)); названия флагов каналов спотера в
  библиотеке совпадают с ней по смыслу. Номера 2, 6, 9 проверены, 3, 5, 7, 8 — по порядку
  имён enum (вывод).

  | Тип | Флаг в `flags.txt` модели | Выход VQE |
  |---|---|---|
  | 0 | основной (`omni-channel`) | 6 `VQE_OMNI` |
  | 1 | `beamforming-channels` | 7 `VQE_BEAMFORMING` |
  | 2 | `feedback-channel` | 5 `RAW_FEEDBACK_SYNC` |
  | 3 | `bnr-channel` | 8 `VQE_BACKGROUND_NOISE_REDUCER` |
  | 4 | `raw-mic-channels`, `freq-filter-audio-channel raw-mic` | 2 `RAW_MAIN_MIC_SYNC` |
  | 5 | — | 3 `RAW_AUXILIARY_MIC_SYNC` |

  Типы 3 и 4 подтверждены парами «флаг — запрошенный тип» у `alisa` Миди и Лайт 2, остальные
  флаги — по именам. Сток подаёт спотеру `vqe_0` (выход 6), а не `vqe_gain` (`quasar.cfg`:
  `audiosender.spotterChannelName`; что без своего усиления — вывод: перевод float → s16 в
  `maind` не декомпилирован); `vqe_gain` (выход 9 = выход 6 ×
  `omniChannelGainCoefficient` пресета, у `cucumber` 50; AGC пресета выключен) — только SIP
  (`audio_source.channel`). Проверено: Миди, модель `alisa` ([midi.md, «Входы спотера:
  замеры»](midi.md#входы-спотера-замеры)); Лайт 2, модель `alisa` — только наличие выходов
  (ww_offline на шуме); `yandex`, командная модель и остальные станции — нет.
- События спотера (`event_id_to_str`): 2 и 5 — срабатывания (`CONFIRMATION_1ST_STAGE` и
  `_2ND_STAGE`), 3 — `1ST_STAGE_HIT` (верификатор ждёт ещё данных), 6 — отказ второй стадии.
- `int spotter_get_unhandled_nbytes(spotter*, spotter_error**)` — точка хита: байты
  чередованного s16-входа всех `nch` каналов спотера от конца окна анализа (25 мс) кадра хита до
  конца всего поданного звука. Кадр спотера — 10 мс; хит — пик уверенности (в стоковых моделях
  `hit-at-peak subhit`, `new-unhandled-nbytes true`). Значение кратно `2·nch` и отрицательно, если
  хит пришёлся на последние ~3 кадра. Опция модели `unhandled-frames-shift` сдвигает точку на
  столько кадров назад (в стоковых `flags.txt` её нет). Вызывать только сразу после события 2/5 и
  `spotter_get_current_phrase`, до следующего `spotter_get_next_event` или подачи: без результата
  хита библиотека делает `abort()` (`Y_VERIFY("resultIdx != -1")`). Так же устроен внутренний
  `FeedData` библиотеки. Символ есть в сборках Миди (aarch64), Лайт 2 и kiosk-alisa (armhf); Мини,
  Мини 2, Мини 3, Мини 3 Про и Станция 2 не смотрелись.
- `libquasar_gstreamer.so` — GStreamer 1.14.5 из прошивки, тоже загружается через `dlopen`. При
  переходе PLAYING → PAUSED его alsasink делает `snd_pcm_drop`: на Миди каждая пауза съедала около
  200 мс ответа. С `sync=true` опоздавшие сэмплы отбрасываются. В сборке Миди нет экспорта ни
  appsink API, ни `gst_buffer_map`; `parse_launch`, `g_object_get` / `g_object_set` и
  `gst_bin_get_by_name` есть на всех моделях.

## alsa-lib: версионированная и неверсионированная

alsa-lib на armhf-образах Миди версионированная (ALSA_0.9 + ALSA_0.9.0rc4). Там обычные имена
функций `snd_pcm_hw_params_*` (get и `set_*_near`) могут привязаться к compat-записям ALSA_0.9,
которые принимают значения вместо указателей: period и buffer читаются как 0, и wake word не
работает. Текущий ABI доступен под алиасами `__snd_pcm_hw_params_*`. У неверсионированной
библиотеки Лайт 2 таких алиасов нет, и текущий ABI там под обычными именами.

## RAM и tmpfs

`/tmp` на станциях — tmpfs: тестовые файлы в нём занимают RAM.

| Модель | RAM (MemTotal) | Источник |
|---|---|---|
| Миди | ~974 МиБ | вики (MemTotal 997788 kB) |
| Лайт 2, плата `plum` | ~237 МиБ | мейнтейнер |
| Лайт 2, плата `prune` | ~238 МиБ, tmpfs ~119 МиБ | #57 (MemTotal 243512 kB) |
| Станция 2 | ~490 МиБ | вики (MemTotal 501664 kB) |
| Мини 2 | ~238 МиБ | вики (MemTotal 243996 kB) |
| Мини 3 | ~238 МиБ | вики (MemTotal 243908 kB) |
| Мини 3 Про | ~473 МиБ | вики (MemTotal 484784 kB) |
| Мини | ~236 МиБ, tmpfs ~118 МиБ | вики (MemTotal 242028 kB); tmpfs — #35 |

## Зависший dmix lock

Зависший dmix lock (stale dmix lock): если процесс убит во время воспроизведения, все следующие
плееры молчат, пока lock не снят (`supervise.sh` снимает его перед каждым запуском демона), иначе —
до перезагрузки. Причина — dmix в alsa-lib
прошивки: shm-сегмент остаётся со счётчиком клиентов, который уже никто не уменьшит. Механизм
подробно описан в заголовке
[`../satellite/deploy/supervise.sh`](../satellite/deploy/supervise.sh); там он прописан для Миди и
Лайт 2.

Ключи IPC:

- `speakers` прошивки (Миди, Лайт 2, Мини 3 Про), `dmix_speakers` Мини 3 и `dmix_spk` Станции 2
  (`ipc_key 1024`): shm 0x400, 0x401 (буфер микширования), семафор 0x400;
- `minidmix` у Мини (`ipc_key 2277` в [`../satellite/deploy/asound.conf`](../satellite/deploy/asound.conf)):
  shm 0x8e5, 0x8e6, семафор 0x8e5;
- `dmix_spk_plug` у Мини 2: ключ не сверен.

Вручную lock снимают так: `ipcrm -M 0x400 -M 0x401 -S 0x400` (у Мини —
`ipcrm -M 0x8e5 -M 0x8e6 -S 0x8e5`), и только если `ipcs -m` показывает nattch 0 у обоих
сегментов.
