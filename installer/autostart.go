package main

import (
	"fmt"
	"regexp"
	"strings"
)

// Lines that start pulsar / dropbear at boot: the body of S99zpulsar. Older installers put the same
// lines into /data/quasar/rootkeeper.sh (removed by unhookRootkeeper).
const (
	hookSat = `[ -x /data/pulsar/boot.sh ] && /data/pulsar/boot.sh </dev/null >/dev/null 2>&1 &`
	hookSSH = `[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &`
)

// rcS runs /etc/init.d/S??* in order with "start" and waits for each. The name sorts after the
// firmware's S97quasar / S99quasar (S99pulsar would sort before S99quasar). The only file pulsar
// writes outside /data.
const initPath = "/etc/init.d/S99zpulsar"

// initScript returns the content of initPath. rcS has no controlling tty (inittab ::sysinit:), so the
// background hooks get no SIGHUP when it exits.
func initScript(withSSH bool) string {
	var b strings.Builder
	b.WriteString("#!/bin/sh\n")
	b.WriteString("# pulsar autostart, written by pulsar-install; remove to disable autostart.\n")
	b.WriteString("[ \"$1\" = start ] || exit 0\n")
	if withSSH {
		b.WriteString(hookSSH + "\n")
	}
	b.WriteString(hookSat + "\n")
	return b.String()
}

var verityRe = regexp.MustCompile("(?i)verity|dm=")

// rootfsGuard returns "" if the root filesystem may be written, else the reason it must not be:
// dm-verity or any device-mapper target (writing under it corrupts the verified image), or /etc not
// on the filesystem mounted at / together with the firmware. cmdline = /proc/cmdline, mapper =
// `ls /dev/mapper`, df = `df /etc /system/vendor/quasar`.
func rootfsGuard(cmdline, mapper, df string) string {
	for _, tok := range strings.Fields(cmdline) {
		if verityRe.MatchString(tok) {
			return "kernel command line has " + tok
		}
	}
	var names []string
	for _, n := range strings.Fields(mapper) {
		if n != "control" {
			names = append(names, n)
		}
	}
	if len(names) > 0 {
		return "device-mapper devices present: " + strings.Join(names, " ")
	}
	rest := df
	if strings.HasPrefix(rest, "Filesystem") {
		_, rest, _ = strings.Cut(rest, "\n")
	}
	// Fields also joins the lines df wraps after a long device name.
	f := strings.Fields(rest)
	if len(f) != 12 {
		return "cannot parse df output"
	}
	if f[0] != f[6] || f[5] != f[11] {
		return fmt.Sprintf("/etc (%s on %s) and /system/vendor/quasar (%s on %s) are on different filesystems", f[0], f[5], f[6], f[11])
	}
	if f[5] != "/" {
		return "/etc is on " + f[5] + ", not on the root filesystem"
	}
	return ""
}

// rootMountRW reports whether / is mounted read-write according to /proc/mounts. The last entry for
// / wins: an initramfs "rootfs / rootfs rw" line can come before the real one.
func rootMountRW(mounts string) (rw, found bool) {
	for _, l := range strings.Split(mounts, "\n") {
		f := strings.Fields(l)
		if len(f) < 4 || f[1] != "/" {
			continue
		}
		opt, _, _ := strings.Cut(f[3], ",")
		rw, found = opt == "rw", true
	}
	return rw, found
}
