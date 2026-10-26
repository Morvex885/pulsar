package main

import (
	"fmt"
	"slices"
	"strings"
)

// Devicetree codename -> name. Only models listed in abis get installed: the daemon drives
// model-specific hardware (mic array + VQE preset, LEDs, clock, inputs).
func modelName(code string) string {
	switch code {
	case "cucumber":
		return "Yandex Station Midi"
	case "plum":
		return "Yandex Station Lite 2"
	case "station-mini2":
		return "Yandex Station Mini 2"
	case "bergamot":
		return "Yandex Station Mini 3"
	}
	return ""
}

// abis lists the userspace ABIs a model's firmware runs, its usual one first; nil = model not
// supported. The installed binary is build/<arch>/pulsar for the first ABI whose dynamic loader
// is on the station. A second ABI is only for a userspace actually seen on that model: some Midi
// images run a 32-bit userspace on the 64-bit kernel (#13) and need the armhf build (its ALSA
// ABI is the one in audio/alsa_abi.h, #14). Lite 2, Mini 2 and Mini 3 stay armhf-only: no 64-bit
// userspace has been seen on them, and the aarch64 binary needs glibc 2.34 — on a miss it would
// crash-loop back to the Yandex stack (no .prev on a first install).
func abis(code string) []string {
	switch code {
	case "cucumber":
		return []string{"aarch64", "armhf"}
	case "plum", "station-mini2", "bergamot":
		return []string{"armhf"}
	}
	return nil
}

// archOf picks the model's first ABI whose loader is on the station (loaders: the words the
// loader probe printed). "" = none of the model's ABIs: skip the station.
func archOf(code string, loaders []string) string {
	for _, a := range abis(code) {
		if slices.Contains(loaders, a) {
			return a
		}
	}
	return ""
}

// hasZigbee: the model has the Zigbee radio the daemon can run as a router (radio/), so the router
// firmware image is installed next to the daemon.
func hasZigbee(code string) bool { return code == "cucumber" }

type station struct {
	serial, arch string
	zigbee       bool
}

// found: installable stations, in the order printed by probe.
var found []station

// stationCode picks the model from the devicetree compatible list (NUL-separated, one entry per
// line or space-separated here). The first entry with a supported codename wins: the list starts
// with board-specific variants, e.g. "yandex,cucumber-dvt2" before "yandex,cucumber" on the Midi.
// The first codename is returned even when nothing is supported, so that probe can name the
// unknown device.
func stationCode(compat string) (string, bool) {
	var first string
	for _, entry := range strings.Fields(compat) {
		code, ok := strings.CutPrefix(entry, "yandex,")
		if !ok || code == "" {
			continue
		}
		if first == "" {
			first = code
		}
		if abis(code) != nil {
			return code, true
		}
	}
	return first, false
}

// probe prints one status line for s and records it in found if it is installable. A device we
// connected to ourselves is disconnected again when it is not.
func probe(s string, ours bool) {
	skip := func(msg string) {
		fmt.Printf("  %s: %s\n", s, msg)
		if ours {
			disconnect(s)
		}
	}
	compat, err := sh(s, `tr '\0' '\n' </proc/device-tree/compatible`)
	if err != nil {
		skip("adb not usable (unauthorized or offline)")
		return
	}
	code, supported := stationCode(compat)
	if code == "" {
		skip("not a Yandex Station")
		return
	}
	name := modelName(code)
	if name == "" {
		name = "unknown Yandex device"
	}
	name += " (" + code + ")"
	if !supported {
		skip(name + " — not supported yet, skipped")
		return
	}
	if uid, _ := sh(s, "id -u"); uid != "0" {
		skip(name + " — adb is not root, skipped")
		return
	}
	// `; true`: a missing loader is not an error. Without it the last [ -x ] exits 1 on a
	// normal Midi (no armhf loader) and sh() reports a failure that is not an adb failure.
	loaders, err := sh(s, `[ -x /lib/ld-linux-aarch64.so.1 ] && echo aarch64; [ -x /lib/ld-linux-armhf.so.3 ] && echo armhf; true`)
	if err != nil {
		skip("adb not usable (unauthorized or offline)")
		return
	}
	arch := archOf(code, strings.Fields(loaders))
	if arch == "" {
		skip(name + " — unsupported userland ABI, skipped")
		return
	}
	var state string
	if _, err := sh(s, "pidof pulsar >/dev/null"); err == nil {
		state = "satellite running"
	} else if _, err := sh(s, "[ -x "+dataDir+"/pulsar ]"); err == nil {
		state = "satellite installed, not running"
	} else {
		state = "stock Yandex"
	}
	found = append(found, station{s, arch, hasZigbee(code)})
	note := ""
	if arch != abis(code)[0] {
		note = ", " + arch + " build"
	}
	fmt.Printf("  [%d] %s: %s — %s%s\n", len(found), s, name, state, note)
}
