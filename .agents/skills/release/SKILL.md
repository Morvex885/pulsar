---
name: release
description: Выпуск релиза pulsar (тег vX.Y.Z, GitHub release). Только по явной просьбе пользователя. Ключевые слова: release, tag, version bump, git tag, gh release, publish, SHA256SUMS, release assets.
---

# Релиз

Только по явной просьбе пользователя. Контракты (имена ассетов, версия, обновления из HA) —
`docs/development.md`, «CI и релизы» и «Обновления из HA»; правила git — `AGENTS.md`, «Git».

## 1. Предусловия

- Ветка `main`, чистое дерево, `HEAD` = `origin/main` (сначала `git fetch origin`).
- Run `build` на этом коммите зелёный:
  `gh run list --branch main --workflow build --commit "$(git rev-parse HEAD)" --limit 1`.
  Не зелёный или его нет — остановиться.

## 2. Проверки

1. `tools/check-authorship.sh $(git describe --tags --abbrev=0 --match 'v*')..HEAD` — код 0.
2. Skill `pre-publish-review` по тому же диапазону (от прошлого тега).

## 3. Версия

`git log --oneline <прошлый тег>..HEAD`: есть `feat:` → minor, иначе patch. Предложить версию
пользователю и дождаться подтверждения.

## 4. Тег (необратимо)

1. `git tag -a vX.Y.Z -m vX.Y.Z` (аннотированный, как прошлые теги).
2. **Повторное явное подтверждение**: после push тег нельзя сдвинуть или удалить. Отказ — удалить
   локальный тег: `git tag -d vX.Y.Z`.
3. `git push origin vX.Y.Z`.

## 5. Контроль

1. Run по тегу (`gh run watch` без ID в неинтерактивной сессии не работает):
   `id=$(gh run list --workflow build --branch vX.Y.Z --limit 1 --json databaseId -q '.[0].databaseId')`,
   затем `gh run watch "$id" --exit-status`. Сразу после push run может ещё не появиться (`id`
   пуст): подождать и повторить `gh run list`.
2. Упал — `gh run view "$id" --log-failed`, сообщить пользователю, релиз не править молча.
3. `gh release view vX.Y.Z --json assets`: набор ассетов — как в `docs/development.md`,
   «CI и релизы», и в `.github/workflows/build.yml`, job `release` (шаг `gh release create`).
   Лишний или недостающий ассет — сообщить пользователю.
