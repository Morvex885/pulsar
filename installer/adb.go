package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
)

// findAdb sets adbPath: adb from PATH, else platform-tools/ next to the payload, else downloaded there.
func findAdb() bool {
	if p, err := exec.LookPath("adb"); err == nil {
		adbPath = p
		return true
	}
	exe := "adb"
	if runtime.GOOS == "windows" {
		exe = "adb.exe"
	}
	dest := filepath.Join(root, "platform-tools")
	local := filepath.Join(dest, exe)
	if _, err := os.Stat(local); err == nil {
		adbPath = local
		return true
	}
	pkg, ok := ptPackages[runtime.GOOS]
	if !ok || (runtime.GOOS == "linux" && runtime.GOARCH != "amd64") {
		fmt.Fprintln(os.Stderr, "adb not found (Android platform-tools): install it, e.g. sudo apt install adb")
		return false
	}
	url := ptBase + pkg.file
	fmt.Fprintf(os.Stderr, "adb not found: downloading Android SDK Platform-Tools 37.0.1 from %s (terms: https://developer.android.com/studio/terms)\n", url)
	if err := fetchPlatformTools(url, pkg.size, pkg.sha1, dest); err != nil {
		fmt.Fprintf(os.Stderr, "platform-tools: %v; install adb yourself (Android platform-tools)\n", err)
		return false
	}
	adbPath = local
	return true
}

// startServer starts the adb server with our stderr, not a pipe: a server spawned by a captured
// adb call would inherit the pipe and keep it open (blocking that call on Windows).
func startServer() bool {
	cmd := exec.Command(adbPath, "start-server")
	cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
	if err := cmd.Run(); err != nil {
		fmt.Fprintln(os.Stderr, "adb start-server failed")
		return false
	}
	return true
}

func adbCmd(args ...string) *exec.Cmd { return exec.Command(adbPath, args...) }

func push(s, local, remote string) error {
	c := adbCmd("-s", s, "push", local, remote)
	c.Stderr = os.Stderr
	return c.Run()
}

func pull(s, remote, local string) error {
	c := adbCmd("-s", s, "pull", remote, local)
	c.Stderr = os.Stderr
	return c.Run()
}

func connect(s string)    { _ = adbCmd("connect", s).Run() }
func disconnect(s string) { _ = adbCmd("disconnect", s).Run() }

var networkSerial = regexp.MustCompile(`:[0-9]+$`)

// devices returns the serials adb lists in state `device`.
func devices() []string {
	b, _ := adbCmd("devices").Output()
	var r []string
	for i, l := range strings.Split(strings.ReplaceAll(string(b), "\r", ""), "\n") {
		if f := strings.Fields(l); i > 0 && len(f) >= 2 && f[1] == "device" {
			r = append(r, f[0])
		}
	}
	return r
}

func networkDevices() []string {
	var r []string
	for _, s := range devices() {
		if networkSerial.MatchString(s) {
			r = append(r, s)
		}
	}
	return r
}

func isConnected(s string) bool {
	for _, d := range devices() {
		if d == s {
			return true
		}
	}
	return false
}

var errNoStatus = errors.New("no exit status")

// shRaw runs cmd on the device and returns its output (newlines as sent) and an error for adb
// failures or a non-zero remote status. adb shell on these firmwares always exits 0 and adds \r:
// the remote status is carried in the output.
func shRaw(serial, cmd string) (string, error) {
	b, err := adbCmd("-s", serial, "shell", cmd+"\necho __rc=$?").Output()
	if err != nil {
		return "", err
	}
	o := strings.TrimRight(strings.ReplaceAll(string(b), "\r", ""), "\n")
	i := strings.LastIndex(o, "__rc=")
	if i < 0 {
		return "", errNoStatus
	}
	out, rc := o[:i], o[i+len("__rc="):]
	if rc != "0" {
		return out, fmt.Errorf("exit %s", rc)
	}
	return out, nil
}

// sh is shRaw with trailing newlines removed (for captured output).
func sh(serial, cmd string) (string, error) {
	out, err := shRaw(serial, cmd)
	return strings.TrimRight(out, "\n"), err
}

// shTo runs cmd and copies its output to w.
func shTo(w io.Writer, serial, cmd string) error {
	out, err := shRaw(serial, cmd)
	io.WriteString(w, out)
	return err
}

// firstField is the text up to the first space (sha256sum output → digest).
func firstField(s string) string {
	f, _, _ := strings.Cut(s, " ")
	return f
}
