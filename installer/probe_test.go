package main

import "testing"

// The devicetree compatible list as `tr '\0' '\n'` leaves it, for every supported model.
func TestStationCode(t *testing.T) {
	cases := []struct {
		compat, code string
		supported    bool
	}{
		// Board-specific variants come first; only the exact supported codename may win.
		{"yandex,cucumber-dvt2\nyandex,cucumber\namlogic, a5", "cucumber", true},
		{"yandex,plum\namlogic, axg", "plum", true},
		// Mini 2: the codename contains a hyphen.
		{"yandex,station-mini2\namlogic, axg", "station-mini2", true},
		// The old separator (spaces) works too.
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

func TestSupportedModels(t *testing.T) {
	cases := []struct{ code, arch, name string }{
		{"cucumber", "aarch64", "Yandex Station Midi"},
		{"plum", "armhf", "Yandex Station Lite 2"},
		{"station-mini2", "armhf", "Yandex Station Mini 2"},
	}
	for _, c := range cases {
		if got := archOf(c.code); got != c.arch {
			t.Errorf("archOf(%q) = %q, want %q", c.code, got, c.arch)
		}
		if got := modelName(c.code); got != c.name {
			t.Errorf("modelName(%q) = %q, want %q", c.code, got, c.name)
		}
	}
	if archOf("station") != "" || modelName("station") != "" {
		t.Error(`"station" must stay unsupported`)
	}
}
