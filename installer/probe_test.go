package main

import "testing"

func TestMiniModel(t *testing.T) {
	if got := modelName("yandexmini"); got != "Yandex Station Mini" {
		t.Fatalf("modelName(yandexmini) = %q", got)
	}
	if hasZigbee("yandexmini") {
		t.Fatal("hasZigbee(yandexmini) = true")
	}
}

func TestArchOfUsesAvailableUserlandABI(t *testing.T) {
	tests := []struct {
		name, code string
		loaders    []string
		want       string
	}{
		{"cucumber prefers aarch64", "cucumber", []string{"aarch64", "armhf"}, "aarch64"},
		{"cucumber falls back to armhf userspace", "cucumber", []string{"armhf"}, "armhf"},
		{"plum stays armhf", "plum", []string{"aarch64", "armhf"}, "armhf"},
		{"plum without its loader is skipped", "plum", []string{"aarch64"}, ""},
		{"station-mini2 is armhf", "station-mini2", []string{"aarch64", "armhf"}, "armhf"},
		{"station-mini2 without its loader is skipped", "station-mini2", []string{"aarch64"}, ""},
		{"bergamot is armhf", "bergamot", []string{"aarch64", "armhf"}, "armhf"},
		{"bergamot without its loader is skipped", "bergamot", []string{"aarch64"}, ""},
		{"mini is armhf", "yandexmini", []string{"aarch64", "armhf"}, "armhf"},
		{"mini without its loader is skipped", "yandexmini", []string{"aarch64"}, ""},
		{"no compatible loader", "cucumber", nil, ""},
		{"unsupported model", "unknown", []string{"aarch64", "armhf"}, ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := archOf(tt.code, tt.loaders); got != tt.want {
				t.Fatalf("archOf(%q, %v) = %q, want %q", tt.code, tt.loaders, got, tt.want)
			}
		})
	}
}

func TestQuasarDeviceType(t *testing.T) {
	cases := map[string]string{
		`{"deviceType":"yandexmini"}`:              "yandexmini",
		"{\n  \"deviceType\" : \"yandexmini\",\n}": "yandexmini",
		`{"deviceType": "station-mini2"}`:          "station-mini2",
		`{"other":"yandexmini"}`:                   "",
		`{"deviceType" "yandexmini"}`:              "",
		`{"deviceType": yandexmini}`:               "",
		``:                                         "",
	}
	for cfg, want := range cases {
		if got := quasarDeviceType(cfg); got != want {
			t.Errorf("quasarDeviceType(%q) = %q, want %q", cfg, got, want)
		}
	}
}

// The devicetree compatible list as `tr '\0' '\n'` leaves it.
func TestStationCode(t *testing.T) {
	cases := []struct {
		compat, code string
		supported    bool
	}{
		// Board-specific variants come first; only the exact supported codename may win.
		{"yandex,cucumber-dvt2\nyandex,cucumber\namlogic, a5", "cucumber", true},
		{"yandex,plum\namlogic, axg", "plum", true},
		{"yandex,bergamot-evt1\nyandex,bergamot\namlogic, axg", "bergamot", true},
		{"yandex,bergamot-evt1\namlogic, axg", "bergamot-evt1", false},
		// Mini 2: the codename contains a hyphen.
		{"yandex,station-mini2\namlogic, axg", "station-mini2", true},
		// adb's pty line ends and the old separator (spaces) work too.
		{"yandex,station-mini2\r\namlogic, axg\r\n", "station-mini2", true},
		{"yandex,cucumber-dvt2 yandex,cucumber amlogic, a5", "cucumber", true},
		// Some other station: named from its codename, then skipped as unsupported.
		{"yandex,station\namlogic, axg", "station", false},
		{"yandex,cucumber-dvt2\namlogic, a5", "cucumber-dvt2", false},
		{"amlogic, axg", "", false},
	}
	for _, c := range cases {
		code, supported := stationCode(c.compat)
		if code != c.code || supported != c.supported {
			t.Errorf("%q: got (%q, %v), want (%q, %v)", c.compat, code, supported, c.code, c.supported)
		}
	}
}
