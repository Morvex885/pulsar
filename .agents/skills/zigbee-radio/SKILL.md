---
name: zigbee-radio
description: Прошивка и чтение Zigbee-радио Станции Миди (Telink B91): сборка radio/build.sh, zigbee_probe info/flash, tools/deconz_read.py. Use for zigbee firmware, radio flash, zigbee_probe, ybl, coordinator, router, deconz, Z2M, ZHA.
---

# Zigbee-радио (Миди)

Факты (роли, протокол, граничные случаи) — `docs/zigbee.md`; сборка — `docs/development.md`, «Сборка».
Всё на станции — по skill `station-test` (lock + `stop`).

## 1. Сборка образов

`radio/build.sh` (нужны `libc6:i386`, `unzip`; подробности — комментарий в начале скрипта) →
`radio/out/zigbee-coordinator.ybl`, `radio/out/zigbee-router.ybl`.

## 2. Утилита на станции

`zigbee_probe` из `satellite/crates/pulsar-zigbee/src/bin/zigbee_probe.rs` (`make ARCH=<arch> tools`):

- `zigbee_probe info` — версия работающей прошивки;
- `zigbee_probe [port]` — deCONZ-сервер прошивки-координатора (по умолчанию 6639; демон — 6638);
- `zigbee_probe flash <image.ybl>` — запись образа.

Демон не должен работать в роли Zigbee: оба драйвят один `ttyS2` (`stop` обязателен).

## 3. Чтение (безопасно)

- Только `tools/deconz_read.py` против порта `zigbee_probe` (при остановленном сервисе) или тестового
  экземпляра (16638). **Никогда против 6638** живого демона: подключение вытесняет Z2M/ZHA.
- Запрещены: zigpy, `ControllerApplication`, write parameter, смена network state.

## 4. Прошивка (необратимо)

- Только после явного подтверждения пользователя в этой сессии.
- Шить только образ текущей роли из `/data/pulsar/state.conf` (`zigbee_role=`):
  `pulsar-coordinator` → `zigbee-coordinator.ybl`, `router` → `zigbee-router.ybl`.
  Роль `off` — не шить.
- GPIO по номеру не трогать; сброс и boot-линии знает только `zigbee_probe`.
- После прошивки `zigbee_probe info`: семейство `0x50` — роутер, `0x51` — координатор.
  Другое значение — остановиться, сообщить.
- Завершение — по `station-test`: `start` → `cleanup` → `unlock`.
