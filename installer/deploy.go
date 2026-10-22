package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// build builds missing outputs in a repository checkout; the release archive ships them and no
// sources.
func build() bool {
	sat := filepath.Join(root, "satellite")
	if _, err := os.Stat(filepath.Join(sat, "Makefile")); err != nil {
		return true
	}
	fmt.Println("== build")
	fail := func(err error) bool {
		fmt.Fprintf(os.Stderr, "build: %v (building from source needs Linux or WSL, see README)\n", err)
		return false
	}
	mk := exec.Command("make", "-C", sat, fmt.Sprintf("-j%d", runtime.NumCPU()))
	mk.Stderr = os.Stderr
	if err := mk.Run(); err != nil {
		return fail(err)
	}
	if opts.ssh {
		for _, a := range []string{"aarch64", "armhf"} {
			if st, err := os.Stat(filepath.Join(root, "ssh", "out", a, "dropbearmulti")); err != nil || st.Size() == 0 {
				b := exec.Command(filepath.Join(root, "ssh", "build.sh"))
				b.Stderr = os.Stderr
				if err := b.Run(); err != nil {
					return fail(err)
				}
				break
			}
		}
	}
	return true
}

func sha256File(p string) string {
	f, err := os.Open(p)
	if err != nil {
		return ""
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return ""
	}
	return hex.EncodeToString(h.Sum(nil))
}

// installOne deploys, (re)starts and checks one station; every step checks its own status.
func installOne(st station) bool {
	s := st.serial
	bin := filepath.Join(root, "satellite", "build", st.arch, "pulsar")
	if fi, err := os.Stat(bin); err != nil || fi.Size() == 0 {
		fmt.Fprintf(os.Stderr, "%s: %s missing\n", s, bin)
		return false
	}
	ts := time.Now().Format("20060102150405")
	changed, supChanged, ok := deployFiles(s, bin, ts, st.arch, st.zigbee)
	if !ok {
		return false
	}
	if opts.noEnable {
		return true
	}
	fmt.Printf("== %s: start\n", s)
	// Marker in the log: the health check looks only at what happens after it.
	mark := fmt.Sprintf("=== install-marker %d %d", os.Getpid(), time.Now().Unix())
	_ = shTo(os.Stdout, s, "echo '"+mark+"' >> /tmp/pulsar.log")
	action := "restart"
	if supChanged {
		action = "reload"
	}
	_ = shTo(os.Stdout, s, "if [ -f "+dataDir+"/enabled ] && { pidof supervise.sh || pidof pulsar; } >/dev/null; then "+
		dataDir+"/ctl.sh "+action+"; else "+dataDir+"/ctl.sh enable; fi")
	fmt.Println()
	if !healthCheck(s, changed, mark) {
		return false
	}
	_ = shTo(os.Stdout, s, dataDir+"/ctl.sh status; "+dataDir+"/ctl.sh log 5")
	fmt.Println()
	return true
}

// deployFiles copies the daemon, scripts, the radio's firmware images (zigbee), dropbear (--ssh)
// and boot hooks. changed: the binary was replaced; supChanged: supervise.sh differs from the
// installed one.
func deployFiles(s, bin, ts, arch string, zigbee bool) (changed, supChanged, ok bool) {
	changed, supChanged = true, true
	sshOut := filepath.Join(root, "ssh", "out", arch)
	deploy := filepath.Join(root, "satellite", "deploy")
	fmt.Printf("== %s: daemon (%s)\n", s, arch)
	if shTo(os.Stdout, s, "mkdir -p "+dataDir) != nil {
		return
	}
	want := sha256File(bin)
	have, _ := sh(s, "[ -f "+dataDir+"/pulsar ] && sha256sum "+dataDir+"/pulsar")
	if want == firstField(have) {
		fmt.Println("binary identical to the installed one, not copied (.prev kept)")
		changed = false
	} else {
		if push(s, bin, dataDir+"/pulsar.new") != nil {
			return
		}
		got, _ := sh(s, "sha256sum "+dataDir+"/pulsar.new")
		if want != firstField(got) {
			_ = shTo(os.Stdout, s, "rm -f "+dataDir+"/pulsar.new")
			fmt.Fprintf(os.Stderr, "%s: push corrupted (sha256 mismatch), nothing changed\n", s)
			return
		}
	}
	// Scripts: a running shell reads its script incrementally, so replace them atomically (new inode).
	// A running supervisor keeps executing the old inode: when supervise.sh changed it is reloaded.
	supHave, _ := sh(s, "[ -f "+dataDir+"/supervise.sh ] && sha256sum "+dataDir+"/supervise.sh")
	if firstField(supHave) == sha256File(filepath.Join(deploy, "supervise.sh")) {
		supChanged = false
	}
	for _, f := range []string{"boot.sh", "supervise.sh", "ctl.sh"} {
		if push(s, filepath.Join(deploy, f), dataDir+"/"+f+".new") != nil {
			return
		}
	}
	if shTo(os.Stdout, s, "cd "+dataDir+" && chmod 755 boot.sh.new supervise.sh.new ctl.sh.new &&\n"+
		"     mv boot.sh.new boot.sh && mv supervise.sh.new supervise.sh && mv ctl.sh.new ctl.sh") != nil {
		return
	}
	if changed {
		if shTo(os.Stdout, s, "cd "+dataDir+" && chmod 755 pulsar.new &&\n"+
			"       { [ ! -f pulsar ] || cp -p pulsar pulsar.prev; } &&\n"+
			"       mv pulsar.new pulsar && sync") != nil {
			return
		}
	} else {
		_ = shTo(os.Stdout, s, "sync")
	}

	if zigbee && (!deployRadioImage(s, "zigbee-coordinator.ybl") || !deployRadioImage(s, "zigbee-router.ybl")) {
		return
	}

	if opts.ssh {
		fmt.Printf("== %s: dropbear\n", s)
		if shTo(os.Stdout, s, "mkdir -p /data/dropbear") != nil ||
			push(s, filepath.Join(sshOut, "dropbearmulti"), "/data/dropbear/dropbearmulti.new") != nil ||
			push(s, filepath.Join(sshOut, "sftp-server"), "/data/dropbear/sftp-server.new") != nil ||
			push(s, filepath.Join(root, "ssh", "start.sh"), "/data/dropbear/start.sh") != nil {
			return
		}
		// Running binaries stay busy; mv replaces them atomically.
		if shTo(os.Stdout, s, "cd /data/dropbear && chmod 755 dropbearmulti.new sftp-server.new start.sh &&\n"+
			"       mv dropbearmulti.new dropbearmulti && mv sftp-server.new sftp-server && sync") != nil {
			return
		}
	}

	fmt.Printf("== %s: boot hooks\n", s)
	if !bootHooks(s, ts) {
		return
	}

	if opts.ssh {
		_ = shTo(os.Stdout, s, "pidof dropbear >/dev/null || /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &")
	}
	return changed, supChanged, true
}

// deployRadioImage copies one of pulsar's radio firmware images, read by the daemon only when the
// Zigbee role that needs it is chosen. A checkout without radio/build.sh output keeps the installed
// one.
func deployRadioImage(s, name string) bool {
	img := filepath.Join(root, "radio", "out", name)
	if fi, err := os.Stat(img); err != nil || fi.Size() == 0 {
		fmt.Println("radio/out/" + name + " missing (radio/build.sh), not copied")
		return true
	}
	want := sha256File(img)
	have, _ := sh(s, "[ -f "+dataDir+"/"+name+" ] && sha256sum "+dataDir+"/"+name)
	if firstField(have) == want {
		return true
	}
	if push(s, img, dataDir+"/"+name+".new") != nil {
		return false
	}
	got, _ := sh(s, "sha256sum "+dataDir+"/"+name+".new")
	if firstField(got) != want {
		_ = shTo(os.Stdout, s, "rm -f "+dataDir+"/"+name+".new")
		fmt.Fprintf(os.Stderr, "%s: push corrupted (sha256 mismatch), %s not replaced\n", s, name)
		return false
	}
	return shTo(os.Stdout, s, "cd "+dataDir+" && mv "+name+".new "+name+" && sync") == nil
}

// bootHooks adds the hook lines to rootkeeper.sh (planRootkeeper), keeping a backup.
func bootHooks(s, ts string) bool {
	tmp, err := os.MkdirTemp("", "pulsar-install")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return false
	}
	defer os.RemoveAll(tmp)
	rkFile := filepath.Join(tmp, "rk")
	rk := "#!/bin/sh\n"
	if shTo(os.Stdout, s, "[ -f "+rkPath+" ]") == nil {
		if pull(s, rkPath, rkFile) != nil {
			return false
		}
		b, err := os.ReadFile(rkFile)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			return false
		}
		rk = string(b)
	}
	newRK, added, write := planRootkeeper(rk, opts.ssh)
	if !write {
		fmt.Println("already present in " + rkPath)
		return true
	}
	// Nothing but our hook lines and blank separators may change.
	if bad := checkRootkeeper(rk, newRK); bad != "" {
		fmt.Fprintf(os.Stderr, "%s: unexpected change to %s, not written:\n%s\n", s, rkPath, bad)
		return false
	}
	newFile := filepath.Join(tmp, "rk.new")
	if err := os.WriteFile(newFile, []byte(newRK), 0o644); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return false
	}
	if push(s, newFile, rkPath+".new") != nil {
		return false
	}
	// Checked by the shell that runs it at boot (busybox sh).
	if _, err := sh(s, "sh -n "+rkPath+".new"); err != nil {
		_, _ = sh(s, "rm -f "+rkPath+".new")
		fmt.Fprintf(os.Stderr, "%s: %s.new fails sh -n on the device, not written\n", s, rkPath)
		return false
	}
	bak := rkPath + ".bak-" + ts
	if shTo(os.Stdout, s, "{ [ ! -f "+rkPath+" ] || cp -p "+rkPath+" "+bak+"; } && chmod 755 "+rkPath+".new && mv "+rkPath+".new "+rkPath+" && sync") != nil {
		return false
	}
	if got, _ := sh(s, "sha256sum "+rkPath); firstField(got) != sha256File(newFile) {
		fmt.Fprintf(os.Stderr, "%s: %s on the device differs from the checked file\n", s, rkPath)
		return false
	}
	if shTo(os.Stdout, s, "[ -f "+bak+" ]") == nil {
		fmt.Printf("%s changed, diff against %s on the device:\n", rkPath, bak)
		d, _ := shRaw(s, "diff "+bak+" "+rkPath)
		for _, l := range splitLines(d) {
			fmt.Println("  " + l)
		}
	} else {
		fmt.Printf("created %s with:\n", rkPath)
		for _, l := range added {
			fmt.Println("  " + l)
		}
	}
	return true
}

// healthCheck runs after a (re)start, at ~10 s and again at ~30 s: the process is alive and, in
// the log after the install marker, there is exactly one `=== start` and no crash report, exit or
// supervisor message (a binary that crashes and is restarted in between would otherwise look
// healthy). On failure, offer to put the previous binary back.
func healthCheck(s string, changed bool, mark string) bool {
	var log string
	bad, unknown := false, false
	for round := 1; round <= 2; round++ {
		if round == 1 {
			time.Sleep(10 * time.Second)
		} else {
			time.Sleep(20 * time.Second)
		}
		// The log may have been copy-truncated by supervise.sh meanwhile: the older half is in .1.
		full, _ := sh(s, "cat /tmp/pulsar.log.1 /tmp/pulsar.log 2>/dev/null | tail -n 1500")
		log, _ = sh(s, "tail -n 600 /tmp/pulsar.log")
		if !strings.Contains(log, mark) {
			if strings.Contains(full, mark) {
				log = full
			} else {
				log = ""
			}
		}
		if log == "" {
			log = "(install marker not found in the log, even in .1)"
			unknown = true
			break
		}
		// The old daemon's exit (and the restart) land after the marker: judge only what follows
		// the first `=== start` after it, which is the new daemon's.
		_, log, _ = strings.Cut(log, mark)
		i := strings.Index(log, "=== start")
		if i < 0 {
			bad = true
			log = "(no start after the install marker)" + log
			break
		}
		log = log[i:]
		n := 0
		for _, l := range strings.Split(log, "\n") {
			if strings.HasPrefix(l, "=== start") {
				n++
			}
		}
		if shTo(os.Stdout, s, "pidof pulsar >/dev/null") != nil || n != 1 ||
			strings.Contains(log, "=== CRASH") || strings.Contains(log, "=== exit") || strings.Contains(log, "=== supervise") {
			bad = true
			break
		}
	}
	if unknown {
		fmt.Fprintf(os.Stderr, "%s: health check UNKNOWN: %s; not rolling back automatically\n", s, log)
		return false
	}
	if !bad {
		fmt.Printf("%s: health check ok\n", s)
		return true
	}
	fmt.Fprintf(os.Stderr, "%s: health check FAILED, log:\n", s)
	lines := strings.Split(log, "\n")
	if len(lines) > 15 {
		lines = lines[len(lines)-15:]
	}
	fmt.Fprintln(os.Stderr, strings.Join(lines, "\n"))
	if changed && shTo(os.Stdout, s, "[ -f "+dataDir+"/pulsar.prev ]") == nil {
		ans := "y"
		if !opts.yes {
			ans = readLine("roll back to the previous binary? [Y/n] ")
		}
		if !strings.HasPrefix(ans, "n") && !strings.HasPrefix(ans, "N") {
			_ = shTo(os.Stdout, s, "cd "+dataDir+" && cp -p pulsar.prev pulsar.rb && mv pulsar pulsar.bad &&\n"+
				"        mv pulsar.rb pulsar && sync && ./ctl.sh restart")
			fmt.Fprintf(os.Stderr, "%s: rolled back (bad binary kept as %s/pulsar.bad)\n", s, dataDir)
		}
	}
	return false
}
