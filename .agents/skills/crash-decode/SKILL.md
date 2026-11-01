---
name: crash-decode
description: >-
  Расшифровка краш-отчёта pulsar (=== CRASH) в строки исходников через addr2line и pulsar.debug. Ключевые слова: crash, SIGSEGV, backtrace, addr2line, pulsar.debug, symbolize, crash report, fp chain.
---

# Расшифровка краша

Контракт (non-PIE, абсолютные адреса, `.debug` той же сборки) — `docs/development.md`,
«Диагностика». Формат отчёта — `satellite/crates/pulsar/src/crash.rs` (заголовок и запись полей в
обработчике). Станцию не трогаем: отчёт читаем из лога.

## 1. Найти отчёт

`=== CRASH` в `tools/station.sh <станция> log N` (лог демона) или `test-log N` (лог тестового
экземпляра), под `station-test`, только чтение; либо в `/tmp/pulsar.log`. Лог ротируется: старый
отчёт может лежать в `/tmp/pulsar.log.1`.

## 2. Взять debug-файл той же сборки

- Релиз: ассет `pulsar-<arch>.debug` того тега, который показывает `pulsar --version`.
- Локально: `satellite/build/<arch>/pulsar.debug` **того же коммита**.

## 3. Сверка

`nm pulsar.debug | grep ' T pulsar_crash_handler$'` должен дать адрес = `handler=` из отчёта. Не
совпал — другая сборка, расшифровка бессмысленна: найти правильный `.debug`.

## 4. Расшифровка

`addr2line -f -C -e pulsar.debug <адреса>`, адреса — из строк отчёта (какие поля есть на какой
архитектуре — в заголовке `crash.rs`): `pc`, `lr` и, где есть, цепочка `fp:`.

Если host-`addr2line` не распознаёт архитектуру — `llvm-addr2line` или `<triple>-addr2line` с теми же
флагами.

## 5. Результат

Сводка: сигнал, `addr` (адрес обращения), стек функций с `файл:строка`, версия сборки. Правка кода —
только если пользователь просит.
