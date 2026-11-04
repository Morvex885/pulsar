# Станция 2 (`station-midi`)

Данные контрибьютора, не перепроверены: PR [#58](https://github.com/Morvex885/pulsar/pull/58)
(fenihx), прошивка `0.369.1.46.4793639783.20261020.295`. Статус: экспериментальный. Железо —
[вики](https://hackalice.me/devices/station-2/); здесь только то, чего там нет
([AGENTS.md, «Документация»](../AGENTS.md#документация)).

Init-скрипты, `hardvold.config` и earcons мейнтейнер сверил с дампом system-раздела прошивки
`0.369.1.50.4794649087.20261021.295`. Это образ эмулятора kuyase: его `/etc/asound.conf` заменён
эмулятором и для сверки не годится.

## Userland и хранилище

- Загрузчик — `/lib/ld-linux-armhf.so.3`, glibc 2.30: ставится сборка `armhf`.
- `S29led_panel_midi` загружает модуль панели `gowin_led_device`; `S99zpulsar` сортируется после
  него и после `S99quasar`.

## Звук

- Захват `hw:0,2`: S16_LE, период 512 кадров (`/proc/asound/card0/pcm2c/sub0/hw_params`; то же в
  `quasar.cfg`, `audiod.audioDevice`: `inRate 48000`, `periodSize 512`, `sampleSize 2`).
- Раскладка каналов — по `pcm.hw_feedback_and_mics_raw` (`/etc/asound.conf`, `ttable`):
  микрофоны — каналы 0, 2, 4, 7, референсы — 8, 9 (сигнал TLV320ADC3101, см. вики).
- VQE: пресет `yandexmidi`, 4 микрофона, 2 референса, выход 16 кГц (`quasar.cfg`,
  `audiod.audioDevice.vqe`).
- Воспроизведение: штатный плеер играет в `default` (`quasar.cfg`); ветка `default` —
  `dmix_spk_plug` → `playback_metrics_tap` → `dmix_spk`, без softvol; регулятор `Master` создаёт
  `soft_volume` (вики, «Звук»).
- Громкость: штатный `/usr/bin/hardvold` работает и при остановленном `maind`. По
  `/etc/hardvold.config` он переводит `Master Playback Volume` в `SY6045S Master Playback Volume`
  по кривой −51…0 дБ; регуляторы `SY6045S Ch1/Ch2` не трогает. Запись `SY6045S Master` в обход
  `hardvold` с ним конфликтовала: при одинаковом raw 77 аппаратный Master был 183–185 либо 255.
- Карты `platformToAlsaMap` в `quasar.cfg` нет: по `set_volume.sh` (вики) кривая линейная, raw —
  процент от 255.
- Earcons модели (`common/spotter_start.mp3`, `common/spotter_cancel.mp3`,
  `common/volume_sound.mp3`, `common/timer.wav`, `ru-RU/playback_error.mp3`) в дампе есть. В
  `libquasar_gstreamer.so` собран gst-libav; что он декодирует MP3 — вывод по строкам библиотеки.

## Светодиоды и часы

- Часов нет.
- `/system/vendor/quasar/ledconfigs` пуст: матриц цвета корпуса нет.
- Датчик освещённости — IIO-устройство `ltr308als01`.

## Кнопки и микрофоны

- `midi_touch`: кроме кнопки действия (вики) — `KEY_VOLUMEUP` и `KEY_VOLUMEDOWN`.
- `gpio-keys`: переключатель микрофонов — `SW_MUTE_DEVICE`; отдельного `KEY_MUTE` нет.

## Проверено

Контрибьютор (fenihx), одна станция, прошивка `0.369.1.46.4793639783.20261020.295`:

- Сбор данных read-only: определение модели, формат и раскладка захвата, датчик освещённости,
  геометрия кадра панели, входные устройства, ключ dmix.
- Установка через `pulsar-install`: станция определяется как «Yandex Station 2 (station-midi)»,
  ставится armhf-сборка; демон запускается.
- По описанию #58: панель, кнопки, датчик освещённости, громкость (ползунок HA, кнопки, голос), в
  том числе на низких уровнях (воспроизведение через `default`), ESPHome и Sendspin.

## Не проверено

- Правки ревью мейнтейнера на станции: создание `Master` через `soft_volume`, отрисовка
  2D-эффектов после правки фазы времени, отказ от записи `SY6045S Ch1/Ch2`.
- Автозапуск `S99zpulsar` после перезагрузки, в том числе регулятор `Master` до первого открытия
  `soft_volume`; обновление из HA.
- Порядок каналов пикселя R, G, B, W (в «Что ещё проверить» вики он тоже не сверен).
- Вид 2D-эффектов, анимаций состояния и индикации mute на панели; подстройка яркости панели по
  датчику.
- Аппаратный переключатель микрофонов в pulsar.
- ALSA-переключатель `SPK mute` на hw:0: глушит ли он выход и в каком состоянии он после загрузки.
- Earcons `.mp3` в pulsar.
- Якорь пре-ролла (`spotter_get_unhandled_nbytes`) при этой сборке библиотеки.
- Zigbee: радио на `/dev/ttyS3`, линии GPIO 474/475, прошивка радио в формате GBL (по данным #58);
  в pulsar не поддерживается, загрузчик и восстановление радио не проверялись.
