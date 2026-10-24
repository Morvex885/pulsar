package main

import (
	"fmt"
	"strings"
)

// Devicetree codename -> name / userland ABI (build/<arch>/). Only models listed in archOf get
// installed: the daemon drives model-specific hardware (mic array + VQE preset, LEDs, clock, inputs).
func modelName(code string) string {
	switch code {
	case "cucumber":
		return "Yandex Station Midi"
	case "plum":
		return "Yandex Station Lite 2"
	case "station-mini2":
		return "Yandex Station Mini 2"
	}
	return ""
}

// archOf: devicetree codename -> userland ABI (build/<arch>/); "" = not supported.
func archOf(code string) string {
	switch code {
	case "cucumber":
		return "aarch64"
	case "plum", "station-mini2":
		return "armhf"
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

// stationCode picks the model from the NUL-separated devicetree compatible list (one entry per
// line here). The first entry with a supported codename wins: the list starts with board-specific
// variants, e.g. "yandex,cucumber-dvt2" before "yandex,cucumber" on the Midi. The first codename
// is returned even when nothing is supported, so that probe can name the unknown device.
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
		if archOf(code) != "" {
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
	var state string
	if _, err := sh(s, "pidof pulsar >/dev/null"); err == nil {
		state = "satellite running"
	} else if _, err := sh(s, "[ -x "+dataDir+"/pulsar ]"); err == nil {
		state = "satellite installed, not running"
	} else {
		state = "stock Yandex"
	}
	found = append(found, station{s, archOf(code), hasZigbee(code)})
	fmt.Printf("  [%d] %s: %s — %s\n", len(found), s, name, state)
}
