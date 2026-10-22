package main

import (
	"fmt"
	"strings"
)

// Boot hook lines in /data/quasar/rootkeeper.sh (run by the firmware's onboot.sh).
const (
	hookSat = `[ -x /data/pulsar/boot.sh ] && /data/pulsar/boot.sh </dev/null >/dev/null 2>&1 &`
	hookSSH = `[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &`
)

func splitLines(s string) []string {
	if s == "" {
		return nil
	}
	return strings.Split(strings.TrimSuffix(s, "\n"), "\n")
}

func joinLines(lines []string) string {
	var b strings.Builder
	for _, l := range lines {
		b.WriteString(l)
		b.WriteByte('\n')
	}
	return b.String()
}

// The script is shared with other tooling and ends in an endless `while` loop: code after it never runs.
func isLoop(l string) bool { return strings.HasPrefix(l, "while ") || strings.HasPrefix(l, "while\t") }

// planRootkeeper returns the new script, the hook lines it adds and whether it must be written.
// Hooks go before the first top-level endless loop, else they are appended.
func planRootkeeper(rk string, withSSH bool) (out string, added []string, write bool) {
	if !strings.Contains(rk, "/data/pulsar/boot.sh") {
		added = []string{hookSat}
	}
	if withSSH && !strings.Contains(rk, "/data/dropbear/start.sh") {
		added = append([]string{hookSSH}, added...)
	}
	if len(added) == 0 {
		return rk, nil, false
	}
	lines := splitLines(rk)
	res := make([]string, 0, len(lines)+len(added)+1)
	done := false
	for _, l := range lines {
		if !done && isLoop(l) {
			res = append(res, added...)
			res = append(res, "")
			done = true
		}
		res = append(res, l)
	}
	if !done {
		res = append(res, "")
		res = append(res, added...)
	}
	return joinLines(res), added, true
}

// checkRootkeeper returns "" if new is old with only hook lines and blank lines inserted, else a
// description of the first difference ("- <old line>\n+ <new line>").
func checkRootkeeper(old, new string) string {
	a, b := splitLines(old), splitLines(new)
	i := 0
	for _, l := range b {
		switch {
		case i < len(a) && l == a[i]:
			i++
		case l == hookSat || l == hookSSH || l == "":
		default:
			if i < len(a) {
				return fmt.Sprintf("- %s\n+ %s", a[i], l)
			}
			return "+ " + l
		}
	}
	if i < len(a) {
		return "- " + a[i]
	}
	return ""
}
