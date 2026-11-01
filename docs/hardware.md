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
| Лайт 2 | `plum` | станция мейнтейнера | [station-lite2](https://hackalice.me/devices/station-lite2/) | [lite2.md](lite2.md) |
| Мини 2 | `station-mini2` | контрибьюторы: [#9](https://github.com/Morvex885/pulsar/pull/9), станция из [#38](https://github.com/Morvex885/pulsar/issues/38) | [station-mini2](https://hackalice.me/devices/station-mini2/) | [mini2.md](mini2.md) |
| Мини 3 | `bergamot` | контрибьютор: [#30](https://github.com/Morvex885/pulsar/pull/30) | [station-mini3](https://hackalice.me/devices/station-mini3/) | [mini3.md](mini3.md) |
| Мини | `yandexmini` | контрибьютор: [#35](https://github.com/Morvex885/pulsar/pull/35) | [station-mini](https://hackalice.me/devices/station-mini/) | [mini.md](mini.md) |

Авторитетны данные Миди и Лайт 2; данные контрибьюторов не перепроверены.

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
  типу, задаёт конфиг модели спотера (`spotter_conf_get_required_channel_count`). Спотер типа 0
  получает основной канал VQE.
- События спотера (`event_id_to_str`): 2 и 5 — срабатывания (`CONFIRMATION_1ST_STAGE` и
  `_2ND_STAGE`), 3 — `1ST_STAGE_HIT` (верификатор ждёт ещё данных), 6 — отказ второй стадии.
  Спотер подтверждает слово через несколько чанков после того, как оно закончилось. По замеру
  `frontend_probe` от 2026-10-28 обе модели Лайт 2 и «Яндекс» на Миди события 3 не выдают
  (подтверждают событием 2), а у «Алисы» на Миди событие 3 приходит в одном чанке с 5. Как
  измерять пре-ролл, описано в
  [voice.md, «Пре-ролл: методика измерения»](voice.md#пре-ролл-методика-измерения).
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
| Лайт 2 | ~237 МиБ | мейнтейнер |
| Мини 2 | ~238 МиБ | вики (MemTotal 243996 kB) |
| Мини 3 | ~238 МиБ | вики (MemTotal 243908 kB) |
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

- `speakers` прошивки (Миди, Лайт 2) и `dmix_speakers` Мини 3 (`ipc_key 1024`):
  shm 0x400, 0x401 (буфер микширования), семафор 0x400;
- `minidmix` у Мини (`ipc_key 2277` в [`../satellite/deploy/asound.conf`](../satellite/deploy/asound.conf)):
  shm 0x8e5, 0x8e6, семафор 0x8e5;
- `dmix_spk_plug` у Мини 2: ключ не сверен.

Вручную lock снимают так: `ipcrm -M 0x400 -M 0x401 -S 0x400` (у Мини —
`ipcrm -M 0x8e5 -M 0x8e6 -S 0x8e5`), и только если `ipcs -m` показывает nattch 0 у обоих
сегментов.
