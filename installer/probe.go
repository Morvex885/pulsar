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
	case "plum", "prune":
		return "Yandex Station Lite 2"
	case "station-mini2":
		return "Yandex Station Mini 2"
	case "bergamot":
		return "Yandex Station Mini 3"
	case "yandexmini":
		return "Yandex Station Mini"
	case "station-midi":
		return "Yandex Station 2"
	}
	return ""
}

// abis перечисляет ABI userspace прошивок модели, обычный — первым; nil — модель не
// поддерживается. Ставится build/<arch>/pulsar для первого ABI, чей динамический загрузчик есть
// на станции. Второй ABI — только для userspace, действительно встреченного на этой модели: часть
// образов Миди запускает 32-битный userspace на 64-битном ядре (#13), им нужна armhf-сборка (её
// ABI ALSA — из audio/alsa_abi.h, #14). Лайт 2, Мини 2, Мини 3, Мини и Станция 2 — только armhf:
// 64-битного userspace на них не видели, а aarch64-бинарю нужна glibc 2.34 — при промахе он ушёл
// бы в цикл падений обратно на стек Яндекса (при первой установке .prev нет). armhf-сборка
// нацелена на glibc 2.23 (как у Мини); userland 2.30 у Лайт 2, Мини 2, Мини 3 и Станции 2 её
// тоже запускает.
func abis(code string) []string {
	switch code {
	case "cucumber":
		return []string{"aarch64", "armhf"}
	case "plum", "prune", "station-mini2", "bergamot", "yandexmini", "station-midi":
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

// quasarDeviceType returns the "deviceType" string of quasar.cfg, the firmware's own model
// marker. It is deliberately a tiny parser: the file is JSON-like and only this key is needed.
func quasarDeviceType(cfg string) string {
	const key = `"deviceType"`
	i := strings.Index(cfg, key)
	if i < 0 {
		return ""
	}
	rest, ok := strings.CutPrefix(strings.TrimSpace(cfg[i+len(key):]), ":")
	if !ok {
		return ""
	}
	rest, ok = strings.CutPrefix(strings.TrimSpace(rest), `"`)
	if !ok {
		return ""
	}
	if j := strings.IndexByte(rest, '"'); j >= 0 {
		return rest[:j]
	}
	return ""
}

// hasZigbee: the model has the Zigbee radio the daemon can run as a router (radio/), so the router
// firmware image is installed next to the daemon.
func hasZigbee(code string) bool { return code == "cucumber" }

type station struct {
	serial, arch, code string
	zigbee             bool
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
		// The first-generation Mini has no yandex,* compatible entry. axg_s420_v03 is an
		// Amlogic reference board, not a Yandex marker: require quasar.cfg's deviceType too
		// (the daemon and boot.sh use the same pair).
		dtid, _ := sh(s, `tr -d '\0' </proc/device-tree/amlogic-dt-id`)
		cfg, _ := sh(s, `cat /system/vendor/quasar/quasar.cfg 2>/dev/null`)
		if dtid == "axg_s420_v03" && quasarDeviceType(cfg) == "yandexmini" {
			code = "yandexmini"
			supported = true
		}
	}
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
	found = append(found, station{serial: s, arch: arch, code: code, zigbee: hasZigbee(code)})
	note := ""
	if arch != abis(code)[0] {
		note = ", " + arch + " build"
	}
	fmt.Printf("  [%d] %s: %s — %s%s\n", len(found), s, name, state, note)
}
