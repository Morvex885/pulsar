// pulsar-install finds rooted Yandex Stations on the local network and installs or updates pulsar.
package main

import (
	"bufio"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

const (
	dataDir = "/data/pulsar"
	rkPath  = "/data/quasar/rootkeeper.sh"
)

const usage = `pulsar-install: finds rooted Yandex Stations on the local network and installs or updates pulsar.

  pulsar-install [--list] [--scan] [--ssh] [--no-enable] [--yes] [ip[:port] ...]

Without addresses: finds Yandex Stations via mDNS or, with --scan, by scanning the /24
of the default-route interface for adb (port 5555); connects, identifies the model and offers
the supported, rooted stations found. Per station:
- copies the daemon + scripts to /data/pulsar (scripts via .new + mv; when the binary
  differs from the installed one, the installed one is kept as .prev; an identical one is not
  copied and leaves .prev alone);
- --ssh also installs dropbear (static build from ssh/build.sh) to /data/dropbear;
- writes /etc/init.d/S99zpulsar, which starts pulsar (and dropbear, if installed) at boot after the
  firmware's quasar script; / is remounted read-write only for this write, and not at all when the
  file is already current. Not written when the root filesystem is protected (dm-verity /
  device-mapper, /etc not on it): pulsar then does not start by itself after a reboot (exit status 1
  unless an older hook still starts it);
- then removes the boot hook lines older versions added to /data/quasar/rootkeeper.sh (backup:
  rootkeeper.sh.bak-<timestamp>), leaving the rest of the file as is, and shows the diff;
- enables the satellite (stops the Yandex stack) unless --no-enable; an already running
  satellite is restarted with the new binary and, after ~10 s, checked (process alive, no crash
  report in the log); on failure the previous binary is put back (asked, or automatic with --yes).
In a repository checkout (satellite/Makefile present) the daemon is built first
(make; ssh/build.sh for --ssh). Outside /data only /etc/init.d/S99zpulsar is written. Re-running is safe.
adb: from PATH, else platform-tools/ next to the installer, else downloaded there
from dl.google.com (Android SDK Platform-Tools 37.0.1; Google's terms apply).
--list only shows what was found; --yes installs to every station found without asking.
--scan may reach other Android devices on the network, which can show an adb authorization prompt.
`

type options struct {
	ssh, noEnable, yes, list, scan bool
	targets                        []string
}

var (
	opts    options
	root    string // payload: satellite/, ssh/ (release archive or repository checkout)
	adbPath string
	stdin   = bufio.NewReader(os.Stdin)
)

func main() {
	code := run()
	pauseIfOwnConsole()
	os.Exit(code)
}

var errHelp = errors.New("help")

func parseArgs(args []string) (options, error) {
	var o options
	for _, a := range args {
		switch {
		case a == "--ssh":
			o.ssh = true
		case a == "--no-enable":
			o.noEnable = true
		case a == "-y" || a == "--yes":
			o.yes = true
		case a == "--list":
			o.list = true
		case a == "--scan":
			o.scan = true
		case a == "-h" || a == "--help":
			return o, errHelp
		case strings.HasPrefix(a, "-"):
			return o, fmt.Errorf("unknown option %s", a)
		case strings.Contains(a, ":"):
			o.targets = append(o.targets, a)
		default:
			o.targets = append(o.targets, a+":5555")
		}
	}
	return o, nil
}

// findRoot locates the payload: next to the executable (release archive), else the working
// directory or one of its parents (repository checkout, `go -C installer run .`).
func findRoot() (string, error) {
	has := func(d string) bool {
		_, err := os.Stat(filepath.Join(d, "satellite", "deploy", "boot.sh"))
		return err == nil
	}
	if exe, err := os.Executable(); err == nil {
		if exe, err := filepath.EvalSymlinks(exe); err == nil && has(filepath.Dir(exe)) {
			return filepath.Dir(exe), nil
		}
	}
	if d, err := os.Getwd(); err == nil {
		for {
			if has(d) {
				return d, nil
			}
			p := filepath.Dir(d)
			if p == d {
				break
			}
			d = p
		}
	}
	return "", errors.New("payload not found: run pulsar-install from the unpacked release archive or inside the repository")
}

// readLine prints prompt on stderr and returns the trimmed answer ("" on EOF).
func readLine(prompt string) string {
	fmt.Fprint(os.Stderr, prompt)
	l, err := stdin.ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return ""
	}
	return strings.TrimSpace(l)
}

func run() int {
	var err error
	if opts, err = parseArgs(os.Args[1:]); err != nil {
		if err == errHelp {
			fmt.Print(usage)
			return 0
		}
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	if root, err = findRoot(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	if !findAdb() || !startServer() {
		return 1
	}
	if !opts.list && !build() {
		return 1
	}

	fmt.Println("== looking for stations")
	seen := map[string]bool{}
	targets := opts.targets
	if len(targets) == 0 {
		// Already connected network devices first, then mDNS (or the port scan).
		for _, s := range networkDevices() {
			seen[s] = true
			probe(s, false)
		}
		if opts.scan {
			targets = scan()
		} else {
			targets = mdns()
		}
	}
	for _, s := range targets {
		if seen[s] {
			continue
		}
		seen[s] = true
		ours := false
		if !isConnected(s) {
			connect(s)
			ours = true
		}
		probe(s, ours)
	}
	if len(found) == 0 {
		fmt.Println("no supported rooted station found")
		return 1
	}
	if opts.list {
		return 0
	}

	var chosen []station
	if opts.yes {
		chosen = found
	} else {
		ans := readLine("install to which? (numbers separated by spaces, a = all, Enter = cancel): ")
		if ans == "a" {
			chosen = found
		} else {
			for _, n := range strings.Fields(ans) {
				i, err := strconv.Atoi(n)
				if err != nil || n[0] < '0' || n[0] > '9' || i < 1 || i > len(found) {
					fmt.Fprintf(os.Stderr, "bad choice: %s\n", n)
					return 1
				}
				chosen = append(chosen, found[i-1])
			}
		}
	}
	if len(chosen) == 0 {
		fmt.Println("nothing to do")
		return 0
	}

	failed := 0
	var notes []string
	for _, st := range chosen {
		ok, note, lost := installOne(st)
		if !ok || lost {
			failed = 1
		}
		if note != "" {
			notes = append(notes, st.serial+": "+note)
		}
	}
	if len(notes) > 0 {
		fmt.Println("== autostart summary")
		for _, n := range notes {
			fmt.Println(n)
		}
	}
	return failed
}
