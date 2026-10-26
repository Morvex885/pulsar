# Мини 3 (`bergamot`) — экспериментальный профиль

Статус: **draft, ручной запуск Rust-демона и playback проверены на одной станции**.
Не Mini 3 Pro. Обследованы две Мини 3 с одинаковой прошивкой
`0.369.1.50.4794652521.20261021.117` и совместимостями
`yandex,bergamot-evt1`, `yandex,bergamot`, `amlogic, axg`.
Владелец подтвердил штатный wake → HA Assist → STT/агент → TTS.
Акустическое качество AEC отдельно не измерялось.

## Подтверждено штатной системой

| Узел | Наблюдение / источник |
| --- | --- |
| ABI | Linux 4.9.68 aarch64, loader `/lib/ld-linux-armhf.so.3`, glibc 2.30; installer выбирает только armhf |
| RAM | MemTotal около 238 MiB |
| Capture | `/etc/asound.conf`: `multi_capture` из `hw:0,1`, 8 физических каналов; три микрофона 0/1/2 и обратный сигнал динамика 6 |
| VQE | preset `bergamot`, 3 mic + 1 spk: 768 capture frames @ 48000 Hz → 256 samples @ 16000 Hz, 16 ms |
| Playback | ALSA `speakers` -> `dmix_speakers` -> `hw:0,0`, stereo 48000 Hz |
| Volume | `Master` mono, 0..255; карта штатного конфига 0->0, 1->33, 20->255 |
| Ring | `/dev/fb0`, имя `logo`, 6x1, 24 bpp; `bergamot:<цвет>:logo0..5`, trigger `ledfb-logo`; отдельного логотипа нет |
| Clock | `bergamot:white:clock<A..G><0..3>`, `DH`/`DL`, `DM`, `DEGREE`; присутствие панели проверяется при старте |
| Keys | evdev `aw933xx`, bitmap содержит action/volume up/down; физические нажатия пока не проверены |
| Mic mute | evdev `gpio-keys`, SW_MUTE_DEVICE; начальное состояние прочитано, физическое переключение пока не проверено |
| Lux | IIO `ltr308als01` |
| Earcons | `common/spotter_start.mp3`, `spotter_cancel.mp3`, `volume_sound.mp3`, `timer.wav`, `ru-RU/playback_error.mp3` существуют |
| Models | Штатные каталоги `spotter_models/activation/alisa` и `spotter_models/command` существуют |

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

## До снятия draft

Соблюдать [протокол стенда](stations.md), согласовать окно остановки штатного стека,
сохранить доступ SSH/ADB и подготовить возврат к нему. Сборка — только на локальном ПК.
Не запускать штатный и свой capture одновременно; не начинать с постоянной установки.

- [x] Локальные fmt, host/cross-clippy, Go tests/vet; полный демон с Sendspin и probes собраны для armhf/glibc 2.30.
- [ ] Полный `make check`: на локальном x86_64/glibc 2.43 падает `ambient::tests::rounds_like_the_fused_cxx_build` на 1 ULP; тот же результат на чистой закреплённой базе, без патча Мини 3. Остальные Rust-тесты, включая новые, проходят; эталон округления не менялся.
- [x] ALSA ABI probe, загрузка VQE-пресета и фактические размеры фреймов.
- [ ] Длительный capture/VQE: короткий 20 s smoke без потерь не доказывает суточную устойчивость.
- [x] Штатное wake word, передача речи в HA, ответ TTS на динамике — подтверждено владельцем.
- [ ] AEC при музыке/ответе: сравнить raw mic и выход VQE; не считать успех wake word доказательством AEC.
- [ ] Кольцо: правильные цвета, порядок зон и восстановление штатных triggers.
- [ ] Кнопки action/volume и mic mute, включая запуск с выключенными микрофонами.
- [ ] Часы, яркость и датчик освещённости; вариант без часов отдельно не проверен.
- [ ] MP3/WAV earcons, громкость и возврат прежнего уровня после теста (не выше 0.2).
- [x] Sendspin/Music Assistant, playback/controls, TTS overlay и повторное подключение.
- [ ] Установка, автозапуск после перезагрузки и возврат штатного стека.

В этот PR не входят microWakeWord, openWakeWord и обучение своего слова активации.
