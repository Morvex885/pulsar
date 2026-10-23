package main

import (
	"strings"
	"testing"
)

const testLoop = "while true; do\n  sleep 60\ndone\n"

func TestUnhookRootkeeper(t *testing.T) {
	const L = testLoop
	const S, H = hookSat, hookSSH
	cases := []struct {
		in, out string
		removed int
	}{
		{"#!/bin/sh\necho start\n" + S + "\n\n" + L, "#!/bin/sh\necho start\n" + L, 1},
		{"#!/bin/sh\necho start\n\n" + S + "\n", "#!/bin/sh\necho start\n", 1},
		{"#!/bin/sh\n\n" + H + "\n" + S + "\n", "#!/bin/sh\n", 2},
		// Two installs, the second with --ssh.
		{"#!/bin/sh\n" + S + "\n\n" + H + "\n\n" + L, "#!/bin/sh\n" + L, 2},
		// The owner's own blank lines stay.
		{"#!/bin/sh\necho\n\n" + S + "\n\n" + L, "#!/bin/sh\necho\n\n" + L, 1},
		{"#!/bin/sh\necho\n\n\n" + S + "\n", "#!/bin/sh\necho\n\n", 1},
		// Indented copies inside the loop and foreign dropbear lines stay; a dead hook after the loop goes.
		{
			"#!/bin/sh\n" + S + "\n\nwhile true; do\n  " + S + "\n  sleep 60\ndone\n[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh\n" + S + "\n",
			"#!/bin/sh\nwhile true; do\n  " + S + "\n  sleep 60\ndone\n[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh\n",
			2,
		},
		{"#!/bin/sh\nM=x\n[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh\n" + L, "#!/bin/sh\nM=x\n[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh\n" + L, 0},
		// A missing final newline stays missing.
		{"#!/bin/sh\n" + S + "\necho x", "#!/bin/sh\necho x", 1},
	}
	for i, c := range cases {
		out, removed := unhookRootkeeper(c.in)
		if out != c.out || removed != c.removed {
			t.Errorf("case %d: got removed=%d\n%q\nwant removed=%d\n%q", i+1, removed, out, c.removed, c.out)
		}
		if removed > 0 {
			if d := checkRootkeeper(out, c.in); d != "" {
				t.Errorf("case %d: checkRootkeeper: %s", i+1, d)
			}
		}
	}
}

func TestHookActive(t *testing.T) {
	cases := []struct {
		rk   string
		want bool
	}{
		{"#!/bin/sh\n" + hookSat + "\n" + testLoop, true},
		{"#!/bin/sh\n" + testLoop + hookSat + "\n", false},
		{"#!/bin/sh\n" + hookSSH + "\n" + testLoop, false},
		{"#!/bin/sh\n" + hookSat + "\n", true},
	}
	for i, c := range cases {
		if got := hookActive(c.rk); got != c.want {
			t.Errorf("case %d: got %v, want %v", i+1, got, c.want)
		}
	}
}

func TestCheckRootkeeperRejects(t *testing.T) {
	in := "#!/bin/sh\necho start\n" + hookSat + "\n\n" + testLoop
	out, _ := unhookRootkeeper(in)
	if d := checkRootkeeper(strings.Replace(out, "sleep 60", "sleep 61", 1), in); d == "" {
		t.Error("changed loop body accepted")
	}
	if d := checkRootkeeper(strings.Replace(out, "echo start\n", "", 1), in); d == "" {
		t.Error("removed line accepted")
	}
}
