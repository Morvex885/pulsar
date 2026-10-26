package main

import "testing"

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
		{"bergamot is armhf", "bergamot", []string{"armhf"}, "armhf"},
		{"bergamot never prefers aarch64", "bergamot", []string{"aarch64", "armhf"}, "armhf"},
		{"bergamot without armhf is skipped", "bergamot", []string{"aarch64"}, ""},
		{"bergamot without loaders is skipped", "bergamot", nil, ""},
		{"bergamot board variant is unsupported", "bergamot-evt1", []string{"aarch64", "armhf"}, ""},
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

func TestMini3Model(t *testing.T) {
	if got := modelName("bergamot"); got != "Yandex Station Mini 3" {
		t.Fatalf("modelName(bergamot) = %q", got)
	}
	if got := abis("bergamot"); len(got) != 1 || got[0] != "armhf" {
		t.Fatalf("abis(bergamot) = %v, want [armhf]", got)
	}
	if hasZigbee("bergamot") {
		t.Fatal("bergamot must not install Zigbee firmware")
	}
	for _, code := range []string{"bergamot-evt1", "bergamot-extra", "unknown"} {
		if modelName(code) != "" || abis(code) != nil {
			t.Errorf("%q must remain unsupported", code)
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
		{"yandex,bergamot", "bergamot", true},
		{"yandex,bergamot-evt1\r\nyandex,bergamot\r\n", "bergamot", true},
		{"yandex,bergamot-evt1 yandex,bergamot amlogic, axg", "bergamot", true},
		{"yandex,bergamot-evt1\namlogic, axg", "bergamot-evt1", false},
		{"yandex,bergamot-extra\namlogic, axg", "bergamot-extra", false},
		{"xyandex,bergamot\namlogic, axg", "", false},
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
