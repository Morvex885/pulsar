---
name: zigbee-radio
description: >-
  Сборка и прошивка Zigbee-радио Станции Миди (Telink B91), чтение через zigbee_probe и tools/deconz_read.py. Ключевые слова: zigbee firmware, radio flash, zigbee_probe, ybl, coordinator, router, deconz, Z2M, ZHA.
---

# Zigbee-радио (Миди)

Факты (роли, протокол, образы) — `docs/zigbee.md`. Всё на станции — по skill `station-test`; правила
чтения и прошивки — `docs/stations.md`, пункт «Радио Zigbee».

## 1. Сборка образов

`radio/build.sh` (требования к системе — комментарий в начале скрипта) →
`radio/out/zigbee-coordinator.ybl`, `radio/out/zigbee-router.ybl`.

## 2. Утилита на станции

`zigbee_probe` (`satellite/crates/pulsar-zigbee/src/bin/zigbee_probe.rs`):
`make -C satellite ARCH=<arch> tools` → `satellite/build/<arch>/tools/zigbee_probe`
(`AGENTS.md`, «Сборка и проверки»). Режимы:

- `zigbee_probe info` — версия работающей прошивки;
- `zigbee_probe [port]` — deCONZ-сервер прошивки-координатора (порт по умолчанию — в заголовке
  файла);
- `zigbee_probe flash <image.ybl>` — запись образа; в выводе строка `writing … (version 0x…)`.

Радио держит либо демон, либо тестовый экземпляр, либо `zigbee_probe`: перед ним — `stop` и, если
запускали, `test-stop`. Утилиту и образ отправить командой `push` (попадут в `/tmp/pulsar-test/`),
запуск — `ssh "$PULSAR_MIDI" /tmp/pulsar-test/zigbee_probe …` (то же ssh-назначение, что у
`station.sh`).

## 3. Чтение

Правила чтения — `docs/stations.md`, пункт «Радио Zigbee»: `tools/deconz_read.py` с портом
`zigbee_probe` или тестового экземпляра.

## 4. Прошивка (необратимо)

1. Только после явного подтверждения пользователя в этой сессии; lock и `stop` — по `station-test`.
2. Какой образ можно шить — `docs/stations.md`, пункт «Радио Zigbee»; образы —
   `docs/zigbee.md`, «Образы .ybl и бутлоадер».
3. `push` образа → `zigbee_probe flash /tmp/pulsar-test/<образ>` → `zigbee_probe info`.
4. Проверка: версия в `info` совпадает с `version 0x…` из строки `writing …` команды `flash`.
   Расхождение — остановиться и сообщить.
5. При `start` демон сверяет версию в радио с образом роли `/data/pulsar/zigbee-<роль>.ybl` и при
   расхождении прошивает радио обратно (`decide` в `satellite/crates/pulsar-zigbee/src/service.rs`,
   там же исключения: нет образа, ранее отвергнутая версия). Образ другой версии живёт только до
   такого `start`.
6. Линии reset и boot по номеру GPIO не трогать (`docs/zigbee.md`, «Радио Миди»).
7. Завершение — порядок в `docs/stations.md`, пункт «Lock».
