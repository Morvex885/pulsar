# Мини 3 (`bergamot`) — экспериментальный профиль

Статус: экспериментальный; ручной запуск демона и воспроизведение проверены на одной станции.
Не Mini 3 Pro. Обследованы две Мини 3 с одинаковой прошивкой
`0.369.1.50.4794652521.20261021.117` и совместимостями
`yandex,bergamot-evt1`, `yandex,bergamot`, `amlogic, axg`.
Акустическое качество AEC отдельно не измерялось.

## Подтверждено штатной системой

| Узел | Наблюдение / источник |
| --- | --- |
| ABI | Linux 4.9.68 aarch64, loader `/lib/ld-linux-armhf.so.3`, glibc 2.30; installer выбирает только armhf |
| RAM | MemTotal около 238 MiB |
| Capture | `/etc/asound.conf`: `multi_capture` из `hw:0,1`, 8 физических каналов: микрофоны PDM0–2 на каналах 0/1/2, канал 6 — `HW feedback (TDMB_IN0)`, обратный сигнал усилителя (референс динамика для VQE) |
| VQE | preset `bergamot`, 3 mic + 1 spk: 768 capture frames @ 48000 Hz → 256 samples @ 16000 Hz, 16 ms |
| Playback | ALSA `speakers` -> `dmix_speakers` -> `hw:0,0`, stereo 48000 Hz; у `dmix_speakers` `ipc_key 1024` (= 0x400), так что очистка зависшего dmix-лока в `supervise.sh` и `tools/station.sh` её покрывает |
| Volume | `Master` mono, 0..255; карта штатного конфига 0->0, 1->33, 20->255 |
| Ring | `/dev/fb0`, имя `logo`, 6x1, 24 bpp (AW21018); `bergamot:<цвет>:logo0..5`, trigger `ledfb-logo`; отдельного логотипа нет |
| Clock | продаётся с часами (AW21036) и без них; `bergamot:white:clock<A..G><0..3>`, `DH`/`DL`, `DM`, `DEGREE`; присутствие панели проверяется при старте |
| Keys | evdev `aw933xx`, bitmap содержит action/volume up/down |
| Mic mute | evdev `gpio-keys`, SW_MUTE_DEVICE; начальное состояние прочитано |
| Lux | IIO `ltr308als01` |
| Earcons | `common/spotter_start.mp3`, `spotter_cancel.mp3`, `volume_sound.mp3`, `timer.wav` (по заголовку это MP3), `ru-RU/playback_error.mp3` [inference: GStreamer прошивки декодирует MP3, раз штатные earcons — MP3; в логе pulsar не проверено] |
| Models | Штатные каталоги `spotter_models/activation/alisa` и `spotter_models/command` существуют |
| Пре-ролл | `alisa` 6, `yandex` 8 чанков по 16 мс: длины Лайт 2 во времени, не замерялись |

В обеих станциях совпали SHA256 `libyandex_evil.so`, `libtflite.so`, `quasar.cfg` и
`asound.conf`. Проприетарные библиотеки, модели, сырые конфиги и персональные сведения
в патч не входят. Пресет берётся именно `bergamot`, не от Мини 2/Лайт 2.

## Runtime-проверки

ALSA ABI probe прошёл (`alias=yes`). В 20 s frontend smoke: 1248 аудиоблоков по 256
сэмплов, 624 music frames, 0 dropped; music 31.23 fps. FFT накапливает два VQE-блока,
а wake-level сохраняет прежнее окно 47 × 32 ms и арифметику 512-сэмпловых блоков.
HA подключился по ESPHome API с автоматической выдачей Noise key. TTS через HA доходит
до EOS. Очередь Music Assistant играет через Sendspin FLAC 48 kHz stereo → ALSA
`speakers`: 60.5 s записи в устройство, 0 xruns, в том числе с HA TTS поверх музыки.
Pause/resume, volume и stop проверены через MA; переподключение после рестарта сервера
MA произошло автоматически. Это не акустическое измерение AEC или распознавания речи.

## Не проверено

- AEC при музыке и ответе (сравнение raw-микрофонов и выхода VQE).
- Сырой захват по каналам (`frontend_probe --record`): какой канал какой микрофон.
- Длительная работа capture/VQE (короткий 20 s smoke без потерь не доказывает суточную
  устойчивость).
- Цвета и порядок зон кольца.
- Физические кнопки action/volume и переключатель mic mute, включая запуск с выключенными
  микрофонами.
- Часы, их яркость и датчик освещённости; вариант без часов.
- MP3-earcons в логе pulsar.
- Длины пре-ролла (строки `wake timing:`).
- Установка через `pulsar-install` от начала до конца и автозапуск после перезагрузки.

Источники: замеры автора PR #30 на двух станциях;
[hackalice.me: Станция Мини 3](https://hackalice.me/devices/station-mini3/).
