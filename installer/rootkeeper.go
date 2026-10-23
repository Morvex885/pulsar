package main

import (
	"fmt"
	"slices"
	"strings"
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

// unhookRootkeeper returns rk without the hook lines older installers added and the number of lines
// removed. Hook lines go wherever they are, including dead ones after the loop. Blank lines go only
// in the two shapes older installers wrote: hooks then a blank line right before the first loop, or
// a blank line then hooks at the end of the file. Every other byte stays.
func unhookRootkeeper(rk string) (out string, removed int) {
	lines := splitLines(rk)
	hook := func(i int) bool { return lines[i] == hookSat || lines[i] == hookSSH }
	drop := make([]bool, len(lines))
	for i := range lines {
		if hook(i) {
			drop[i] = true
			removed++
		}
	}
	if removed == 0 {
		return rk, 0
	}
	if loop := slices.IndexFunc(lines, isLoop); loop >= 0 {
		for j := loop - 1; j >= 0 && (hook(j) || lines[j] == ""); j-- {
			if lines[j] == "" && j > 0 && hook(j-1) {
				drop[j] = true
			}
		}
	}
	for j := len(lines) - 1; j >= 0 && (hook(j) || lines[j] == ""); j-- {
		if lines[j] == "" && j+1 < len(lines) && hook(j+1) {
			drop[j] = true
		}
	}
	kept := make([]string, 0, len(lines))
	for i, l := range lines {
		if !drop[i] {
			kept = append(kept, l)
		}
	}
	out = joinLines(kept)
	if !strings.HasSuffix(rk, "\n") {
		out = strings.TrimSuffix(out, "\n")
	}
	return out, removed
}

// hookActive reports whether rk starts pulsar: a hook line before the endless loop (code after it
// never runs).
func hookActive(rk string) bool {
	for _, l := range splitLines(rk) {
		if isLoop(l) {
			return false
		}
		if l == hookSat {
			return true
		}
	}
	return false
}

// checkRootkeeper returns "" if with is without plus only hook lines and blank lines, else a
// description of the first difference ("- <line of without>\n+ <line of with>").
func checkRootkeeper(without, with string) string {
	a, b := splitLines(without), splitLines(with)
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
