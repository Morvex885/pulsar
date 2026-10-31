---
name: station-test
description: Тестирование pulsar на живых станциях Яндекс (Миди, Лайт 2, Мини) через tools/station.sh: lock, stop, тестовый экземпляр на портах +10000, живая подмена swap/restore, cleanup. Use for station test, live station, lock/unlock, test-start, swap, restore, hardware test, esphome_check, dmix lock, volume cap.
---

# Тест на живой станции

Правила — `docs/stations.md` (читать целиком до первого действия), реализация — `tools/station.sh`,
запреты — `AGENTS.md`, «Безопасность станций». Здесь только процедура; факты не дублируются.

## 1. Допуск

- Нужна переменная адреса: `PULSAR_MIDI`, `PULSAR_LITE2` или `PULSAR_MINI` (ssh-назначение), и
  `PULSAR_TAG` (одно слово, чья работа). Адреса в репозиторий не писать.
- Нет переменной адреса → статус `заблокирован: нет PULSAR_<X>`, дальше не идти.
- Вызов: `PULSAR_TAG=<метка> tools/station.sh <midi|lite2|mini> <команда>`. Список команд — usage в
  начале скрипта (`lock`, `unlock`, `break-lock`, `stop`, `start`, `push`, `test-start`,
  `test-stop`, `test-log`, `swap`, `restore`, `cleanup`, `status`, `log`).
- Начать с `status` (ничего не меняет): кто держит lock, режим станции.

## 2. Lock

- `lock <минуты>`: срок честный, 1–180; для живой подмены — не меньше 30.
- Всё, что меняет станцию, — только под своим lock; `break-lock` чужого — по правилам
  `docs/stations.md` (срок + 30 минут), без запроса пользователю не снимать.

## 3. Остановка и аппаратные тесты

- Аппаратные тесты, probe-утилиты и любые тесты ESPHome API — только после `stop`.
- `stop` отказывает в Yandex-режиме (нет `/data/pulsar/enabled`): статус
  `заблокирован: станция в Yandex-режиме`, сообщить владельцу, не обходить.
- Сетевые тесты без API (mDNS, арбитр, HTTPS) допустимы при работающем демоне, но только на
  альтернативных портах и с `_pulsartest._tcp`; порты 6053, 6638, 8928 живого демона не трогать.

## 4. Тестовый экземпляр

1. Собрать: `make -C satellite ARCH=<arch> ...` (см. `docs/development.md`, «Сборка»).
2. `push satellite/build/<arch>/pulsar` (файлы попадают в `/tmp/pulsar-test/`, передача проверяется).
3. `stop`, затем `test-start` (`PULSAR_PORT_OFFSET=10000`, громкость `volume2=0.2`).
4. Клиент: `tools/esphome_check.py --host <ip> --port 16053`; лог: `test-log [N]`.
5. Probe-утилиты (`push` в `/tmp/pulsar-test/`, запуск по ssh) — список в `docs/architecture.md`, «Workspace».

## 5. Звук

- Только с согласия владельца, громкость ≤ 0.2 (Миди/Лайт 2: raw ≤ 82, проверить
  `amixer -c0 cget name='Master Playback Volume'`; Мини: громкость программная, смотреть через API).
- Убит процесс во время звука → обязательно `cleanup` (снимает устаревший dmix-lock), иначе станция
  молчит до перезагрузки.

## 6. Живая подмена (необратимо для владельца)

- `swap <бинарь>` — только после явного подтверждения пользователя в этой сессии; lock ≥ 30 минут.
- Миди в роли `pulsar-coordinator`: дополнительные условия из `docs/stations.md`, «Живая подмена»
  (сверка чтения `tools/deconz_read.py` и подтверждённый бэкап Z2M/ZHA). Нет подтверждения → не
  подменять, пункт в чек-лист владельца.
- После наблюдения за логом (`log [N]`) — обязательно `restore`.

## 7. Завершение

`test-stop` (если запускали) → `start` или `restore` → `cleanup` → `unlock`. Сообщить
пользователю итог и что станция возвращена владельцу.
