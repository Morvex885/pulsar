---
name: crash-decode
description: Расшифровка краш-отчёта pulsar (=== CRASH) в строки исходников через addr2line и pulsar.debug. Use for crash, SIGSEGV, backtrace, addr2line, pulsar.debug, symbolize, crash report, fp chain.
---

# Расшифровка краша

Формат отчёта — `satellite/crates/pulsar/src/crash.rs`; почему адреса абсолютные (non-PIE) —
`satellite/Makefile` (`relocation-model=static`). Станцию не трогаем: отчёт читаем из лога.

## 1. Найти отчёт

`=== CRASH` в `tools/station.sh <станция> log N` (под `station-test`, чтение) или в
`/tmp/pulsar.log`. Строка: `=== CRASH <сигнал> addr=… pc=… lr=… handler=…`; на aarch64 следующая
строка `fp: <адрес> <адрес> …`.

## 2. Взять debug-файл той же сборки

- Релиз: ассет `pulsar-<arch>.debug` тега, который показывает `pulsar --version`.
- Локально: `satellite/build/<arch>/pulsar.debug` **того же коммита** (иначе адреса не совпадут).

## 3. Сверка

`nm pulsar.debug | grep pulsar_crash_handler` должен дать адрес = `handler=` из отчёта. Не совпал →
другая сборка, расшифровка бессмысленна: найти правильный `.debug`.

## 4. Расшифровка

`addr2line -f -C -e pulsar.debug <адреса>`:

- aarch64: `pc`, `lr` и все адреса цепочки `fp:`;
- armhf: `pc` и `lr` (цепочки нет).

Host-`addr2line` не берёт чужую архитектуру → `llvm-addr2line` с теми же флагами.

## 5. Результат

Сводка: сигнал, `addr` (адрес обращения), стек функций с `файл:строка`, версия сборки. Правка кода —
только если пользователь просит.
