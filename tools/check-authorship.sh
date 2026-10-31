#!/bin/sh
# Проверка авторства коммитов диапазона: ни автор, ни коммиттер, ни Co-authored-by,
# ни подпись в сообщении не должны указывать на AI-инструмент.
# Правило: AGENTS.md, раздел «Git»; в CI его исполняет job `commits` (build.yml).
# Скрипт обязан работать в busybox/dash, поэтому только POSIX sh.

if [ $# -ne 1 ]; then
    echo "usage: $0 <A..B>" >&2
    exit 2
fi

# dependabot[bot] сюда не попадает намеренно: это легитимный автор обновлений.
AI='claude|anthropic|codex|openai|chatgpt|copilot|cursor|devin|gemini|aider|kimi|moonshot'
# Рекламные подписи вида «Generated with ...» — нарушение само по себе, даже без имени AI.
SIGN='^[^[:alnum:]]*generated (with|by)'

# Код возврата git теряется в конвейере, поэтому сначала вывод во временный файл.
tmp=$(mktemp) || exit 2
out=$(mktemp) || exit 2
trap 'rm -f "$tmp" "$out"' EXIT HUP INT TERM

if ! git log --format='%H%x1f%an%x1f%ae%x1f%cn%x1f%ce%x1f%B%x1e' "$1" >"$tmp"; then
    echo "check-authorship: git log failed for '$1'" >&2
    exit 2
fi

bad=0
# Записи разделены RS (\036), поля — US (\037).
us=$(printf '\037')
soh=$(printf '\001')

# Одна запись = одна строка: иначе многострочное сообщение не прочитать построчным read.
tr '\n\036' '\001\n' <"$tmp" | while IFS= read -r rec; do
    [ -n "$rec" ] || continue
    # Первый коммит после RS начинается с \001 (перевод строки после предыдущего %x1e).
    case $rec in "$soh"*) rec=${rec#?} ;; esac
    case $rec in *"$us"*) ;; *) continue ;; esac
    rest=$rec
    sha=${rest%%"$us"*};  rest=${rest#*"$us"}
    an=${rest%%"$us"*};   rest=${rest#*"$us"}
    ae=${rest%%"$us"*};   rest=${rest#*"$us"}
    cn=${rest%%"$us"*};   rest=${rest#*"$us"}
    ce=${rest%%"$us"*};   msg=${rest#*"$us"}
    short=$(printf '%s' "$sha" | cut -c1-7)

    for pair in "author:$an" "author:$ae" "committer:$cn" "committer:$ce"; do
        v=${pair#*:}
        if printf '%s\n' "$v" | grep -qiE "$AI"; then
            echo "$short ${pair%%:*}: $v"
            echo bad
        fi
    done

    printf '%s\n' "$msg" | tr '\001' '\n' | while IFS= read -r line; do
        case $line in
            [Cc][Oo]-[Aa][Uu][Tt][Hh][Oo][Rr][Ee][Dd]-[Bb][Yy]:*)
                if printf '%s\n' "$line" | grep -qiE "$AI"; then
                    v=${line#*:}
                    echo "$short co-author: ${v#"${v%%[! ]*}"}"
                    echo bad
                fi ;;
        esac
        if printf '%s\n' "$line" | grep -qiE "$SIGN"; then
            echo "$short message: $line"
            echo bad
        fi
    done
done >"$out"

# Маркер «bad» отделяем от сообщений о нарушениях.
grep -v '^bad$' "$out"
if grep -q '^bad$' "$out"; then bad=1; fi
exit "$bad"
