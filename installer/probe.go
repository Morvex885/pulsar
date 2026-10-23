package main

import (
	"fmt"
	"regexp"
)

// Devicetree codename -> name / userland ABI (build/<arch>/). Only models listed in archOf get
// installed: the daemon drives model-specific hardware (mic array + VQE preset, LEDs, clock, inputs).
func modelName(code string) string {
	switch code {
	case "cucumber":
		return "Yandex Station Midi"
	case "plum":
		return "Yandex Station Lite 2"
	}
	return ""
}

// archOf: devicetree codename -> userland ABI (build/<arch>/); "" = not supported.
func archOf(code string) string {
	switch code {
	case "cucumber":
		return "aarch64"
	case "plum":
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

var codename = regexp.MustCompile(`yandex,([a-z0-9_]*)`)

// probe prints one status line for s and records it in found if it is installable. A device we
// connected to ourselves is disconnected again when it is not.
func probe(s string, ours bool) {
	skip := func(msg string) {
		fmt.Printf("  %s: %s\n", s, msg)
		if ours {
			disconnect(s)
		}
	}
	compat, err := sh(s, `tr '\0' ' ' </proc/device-tree/compatible`)
	if err != nil {
		skip("adb not usable (unauthorized or offline)")
		return
	}
	m := codename.FindStringSubmatch(compat)
	if m == nil || m[1] == "" {
		skip("not a Yandex Station")
		return
	}
	code := m[1]
	name := modelName(code)
	if name == "" {
		name = "unknown Yandex device"
	}
	name += " (" + code + ")"
	if archOf(code) == "" {
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
