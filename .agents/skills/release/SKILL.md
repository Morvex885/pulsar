---
name: release
description: Выпуск релиза pulsar (тег vX.Y.Z, GitHub release). Только по явной просьбе пользователя. Use for release, tag, version bump, git tag, gh release, publish, SHA256SUMS, release assets.
---

# Релиз

Только по явной просьбе пользователя. Контракты (имена ассетов, версия, обновления из HA) —
`docs/development.md`, «CI и релизы» и «Обновления из HA»; правила git — `AGENTS.md`, «Git».

## 1. Предусловия

- Ветка `main`, чистое дерево, `HEAD` = `origin/main` (`git fetch origin` сначала).
- Последний run на main зелёный: `gh run list --branch main --workflow build --limit 1`.
  Не зелёный → остановиться.

## 2. Проверки

1. `tools/check-authorship.sh $(git describe --tags --abbrev=0 --match 'v*')..HEAD` — код 0.
2. Skill `pre-publish-review` по тому же диапазону (от прошлого тега).

## 3. Версия

`git log --oneline <прошлый тег>..HEAD`: есть `feat:` → minor, иначе patch. Предложить
версию пользователю и дождаться подтверждения.

## 4. Тег (необратимо)

1. `git tag -a vX.Y.Z -m vX.Y.Z` (аннотированный: версия берётся из `git describe`).
2. **Повторное явное подтверждение**: после push тег нельзя сдвинуть или удалить.
3. `git push origin vX.Y.Z`.

## 5. Контроль

- `gh run watch` (run по тегу, job `release`).
- `gh release view vX.Y.Z --json assets` — ровно ассеты из `.github/workflows/build.yml`, job
  `release` (шаг `gh release create`: `pulsar-*.bin pulsar-*.debug zigbee-*.ybl pulsar-*.zip
  pulsar-*.tar.gz SHA256SUMS`):
  - `pulsar-aarch64.bin`, `pulsar-armhf.bin`, `pulsar-aarch64.debug`, `pulsar-armhf.debug`;
  - `zigbee-coordinator.ybl`, `zigbee-router.ybl`;
  - `pulsar-windows-amd64.zip`;
  - `pulsar-macos-arm64.tar.gz`, `pulsar-macos-amd64.tar.gz`, `pulsar-linux-amd64.tar.gz`,
    `pulsar-linux-arm64.tar.gz`;
  - `SHA256SUMS`.
- Расхождение (лишний или недостающий ассет) — сообщить пользователю, не править релиз молча.
