package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestInitScript(t *testing.T) {
	sh, shErr := exec.LookPath("sh")
	for _, withSSH := range []bool{false, true} {
		s := initScript(withSSH)
		if !strings.HasPrefix(s, "#!/bin/sh\n") || !strings.HasSuffix(s, "\n") {
			t.Errorf("ssh=%v: bad framing:\n%s", withSSH, s)
		}
		lines := splitLines(s)
		guard := -1
		for i, l := range lines {
			if l == `[ "$1" = start ] || exit 0` {
				guard = i
				break
			}
		}
		sat, ssh := -1, -1
		nsat := 0
		for i, l := range lines {
			switch l {
			case hookSat:
				sat = i
				nsat++
			case hookSSH:
				ssh = i
			}
		}
		if guard < 0 || guard > sat || (ssh >= 0 && guard > ssh) {
			t.Errorf("ssh=%v: start guard missing or after a hook:\n%s", withSSH, s)
		}
		if nsat != 1 || sat != len(lines)-1 {
			t.Errorf("ssh=%v: pulsar hook not exactly once and last:\n%s", withSSH, s)
		}
		if (ssh >= 0) != withSSH || ssh > sat {
			t.Errorf("ssh=%v: dropbear hook wrong:\n%s", withSSH, s)
		}
		if shErr == nil {
			p := filepath.Join(t.TempDir(), "S99zpulsar")
			if err := os.WriteFile(p, []byte(s), 0o755); err != nil {
				t.Fatal(err)
			}
			if out, err := exec.Command(sh, "-n", p).CombinedOutput(); err != nil {
				t.Errorf("ssh=%v: sh -n: %v\n%s", withSSH, err, out)
			}
		}
	}
}

func TestRootfsGuard(t *testing.T) {
	const hdr = "Filesystem           1K-blocks      Used Available Use% Mounted on\n"
	const midiDF = hdr + "/dev/root               998060    389024    540224  42% /\n/dev/root               998060    389024    540224  42% /\n"
	cases := []struct {
		name, cmdline, mapper, df string
		ok                        bool
	}{
		{"midi", "console=ttyS0,921600 root=PARTLABEL=system rootfstype=ext4 ro androidboot.verifiedbootstate=green", "control\n", midiDF, true},
		{"lite2", "root=ubi0:rootfs rootfstype=ubifs", "", hdr + "ubi0:rootfs              71988     62628      9360  87% /\nubi0:rootfs              71988     62628      9360  87% /\n", true},
		{"dm table", `root=/dev/dm-0 dm="1 vroot none ro 1,0 4096 verity 1 /dev/mmcblk0p5"`, "control\n", midiDF, false},
		{"veritymode", "root=/dev/root androidboot.veritymode=enforcing", "control\n", midiDF, false},
		{"mapper", "root=/dev/root", "control\nsystem\n", midiDF, false},
		{"different fs", "", "control", hdr + "/dev/root 998060 389024 540224 42% /\n/dev/mmcblk0p5 998060 389024 540224 42% /system\n", false},
		{"not at /", "", "control", hdr + "/dev/root 998060 389024 540224 42% /rootfs\n/dev/root 998060 389024 540224 42% /rootfs\n", false},
		{"wrapped", "", "control", hdr + "/dev/disk/by-partlabel/system-long-name\n 998060 389024 540224 42% /\n/dev/disk/by-partlabel/system-long-name\n 998060 389024 540224 42% /\n", true},
		{"one line", "", "control", hdr + "/dev/root 998060 389024 540224 42% /\n", false},
	}
	for _, c := range cases {
		r := rootfsGuard(c.cmdline, c.mapper, c.df)
		if (r == "") != c.ok {
			t.Errorf("%s: got %q, want ok=%v", c.name, r, c.ok)
		}
	}
}

func TestRootMountRW(t *testing.T) {
	cases := []struct {
		mounts    string
		rw, found bool
	}{
		{"rootfs / rootfs rw 0 0\n/dev/root / ext4 ro,relatime 0 0\nproc /proc proc rw 0 0", false, true},
		{"ubi0:rootfs / ubifs rw,relatime 0 0", true, true},
		{"proc /proc proc rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n", false, false},
	}
	for i, c := range cases {
		rw, found := rootMountRW(c.mounts)
		if rw != c.rw || found != c.found {
			t.Errorf("case %d: got (%v, %v), want (%v, %v)", i+1, rw, found, c.rw, c.found)
		}
	}
}
