# Мини (`yandexmini`)

Первое поколение. Данные контрибьюторов, не перепроверены: PR
[#35](https://github.com/Morvex885/pulsar/pull/35) (devqcf), затворы выхода —
[#68](https://github.com/Morvex885/pulsar/pull/68) (ART7182) и
[#74](https://github.com/Morvex885/pulsar/issues/74) (kadkin-a), уровни выходов VQE —
[#78](https://github.com/Morvex885/pulsar/pull/78) (artemdanielov). Железо —
[вики](https://hackalice.me/devices/station-mini/); здесь только то, чего там нет
([AGENTS.md, «Документация»](../AGENTS.md#документация)).

## Userland и хранилище

- `amlogic-dt-id = axg_s420_v03` — id референсной платы Amlogic S420, не яндексовский: по нему
  одному Мини не отличить, детект требует второго признака
  ([development.md, «Установка и станция»](development.md#установка-и-станция)).
- 32-битный userland armhf, glibc 2.23.
- `/data` → `/bkupgrade/data` (ubifs, свободно ~61 МБ).

## Звук

- Захват: `hw:0,2`, 8 каналов @ 48 кГц, S32_LE с right-justified 24 битами (к 16 битам для VQE —
  сдвиг на 8): микрофоны 0–3, референсы динамика 6/7; на каналах 4/5 большой постоянный сдвиг (в
  `quasar.cfg` — `micUnknownChannels`), в VQE они не идут.
- Воспроизведение: `mini`: dmix `minidmix` над `hw:0,0` из
  [`../satellite/deploy/asound.conf`](../satellite/deploy/asound.conf) (на станции —
  `/data/pulsar/asound.conf`). `hw:0,0` — единственный выход, петля которого приходит на каналы
  захвата 6/7.
- Громкость: ALSA-мастера, который ослаблял бы выход, нет.
- Затвор выхода — только `/sys/class/tdm_state/amp_table` = 1: усилитель открыт (GPIO `spk_mute`
  поднят, `SPK mute` читается включённым). Так стоит сразу после загрузки штатной прошивки, и стек
  Яндекса этого не меняет, пока играет. Драйвер обнуляет `amp_table` при закрытии PCM, в том числе
  когда dmix закрывает `hw:0,0` после ухода последнего клиента. Затвор, открытый уже после старта
  выхода, даёт звук примерно через 2 с (#74).
- `SPK mute` глушит, как и говорит имя, хотя при открытом усилителе читается включённым: запись 1
  даёт в dmesg `spk_mute_set: mute flag = 1`, опускает GPIO `spk_mute` и сбрасывает `amp_table` в 0
  ([#68](https://github.com/Morvex885/pulsar/pull/68)). В #74 (своя прошивка на Buildroot)
  `amp_table` падает в 0 примерно через 0,2 с после старта, когда следом пишется `SPK mute`, а без
  этой записи держится.
- `aux_amp` — данные расходятся: по #68 запись 1 поднимает `aux_amp_mute` и сбрасывает
  `amp_table`, после загрузки `aux_amp` = 0; в #74 запись `aux_amp` = 1 следом за `amp_table` звук
  не глушила; в #35 в простое под C++-демоном автора (`v0.4.3-mini1`) `aux_amp` = 1.
- Userland штатной прошивки 1.266.4.40 (дамп rootfs, поиск по строкам): путь `amp_table` есть в
  `maind`, `bt_demo` пишет в него `echo 1`/`echo 0`; имён `aux_amp` и `SPK mute` нет ни в одном
  файле. `mv_ioguard` пишет соседний `/sys/class/tdm_state/aux_table` и переключает mixer по
  `numid`; затворы ли это, не проверялось.
- VQE: пресет `yandexmini`: 4 микрофона, 2 референса, 48 кГц, чанк 768 → 256 @ 16 кГц.
- Выходы VQE на командах в HA ([#78](https://github.com/Morvex885/pulsar/pull/78): записи
  `debug_recording_dir`, по 5 команд; релиз 0.6.0 с `VQE_OMNI_WITH_GAIN` и тестовая сборка на его
  базе с `VQE_OMNI`): `VQE_OMNI_WITH_GAIN` — около трети отсчётов в насыщении, фон −13…−19 dBFS,
  STT путает слова; `VQE_OMNI` — насыщения нет, фон около −45 dBFS, речь около −20 dBFS. Какой
  канал сток даёт распознаванию — [hardware.md, «Библиотеки Яндекса (C
  API)»](hardware.md#библиотеки-яндекса-c-api).
- Спотеры: те же модели прошивки; не замерялись.
- alsa-lib версионированная, `/system/vendor/quasar/libs/libasound.so.2` — вне путей поиска
  загрузчика.

## Кнопки и микрофоны

- Кнопки: `adc_keypad`: только `KEY_MUTE` 113; нет ни сенсорной кнопки действия, ни кнопок
  громкости.
- Аппаратного отключения микрофонов нет: кнопка только шлёт `KEY_MUTE`.

## Проверено

Автор #35 на своей станции (до доработки #35): wake word → HA Assist, управление кольцом, TTS через
динамик, подключение ESPHome, Sendspin без xrun.

ART7182 ([#68](https://github.com/Morvex885/pulsar/pull/68)) на YNDX-0004, сборка 0.5.3 с затвором
только `amp_table`: ответы Assist и объявления звучат через динамик.

artemdanielov ([#78](https://github.com/Morvex885/pulsar/pull/78)) на прошивке
`3.211.373.4.16.4829693628.20261104`, тестовая сборка на базе 0.6.0 с `VQE_OMNI` в HA: wake word →
HA Assist (STT, ответ TTS через динамик), таймеры.

## Не проверено

- После доработки #35 железа Мини у мейнтейнера не было: сдвиг `>> 8` на реальном захвате
  (`frontend_probe --record`), RUNPATH у `libquasar_gstreamer.so`, полный цикл на колонке.
- Направление анимаций кольца относительно штатного (порядок зон — на
  [вики](https://hackalice.me/devices/station-mini/)) на колонке не сверялось.
- Якорь пре-ролла (`spotter_get_unhandled_nbytes`) при этой сборке библиотеки.
- Кнопка `KEY_MUTE` (программное отключение микрофонов), программная громкость и затвор
  `amp_table` у музыки (`media_player`) и Sendspin.
- Установка через `pulsar-install` (с `/data/pulsar/asound.conf`) и автозапуск после перезагрузки.
