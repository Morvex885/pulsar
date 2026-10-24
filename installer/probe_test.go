package main

import "testing"

// The devicetree compatible list as `tr '\0' ' '` leaves it, for every supported model.
func TestCodename(t *testing.T) {
	cases := []struct {
		compat, code string
		ok           bool
	}{
		{"yandex,cucumber amlogic, a5 ", "cucumber", true},
		{"yandex,plum amlogic, axg ", "plum", true},
		// Mini 2: the codename contains a hyphen; a pattern without it would cut it to "station".
		{"yandex,station-mini2 amlogic, axg ", "station-mini2", true},
		{"yandex,station ", "station", true}, // some other station: found, then skipped by archOf
		{"amlogic, axg ", "", false},
	}
	for _, c := range cases {
		m := codename.FindStringSubmatch(c.compat)
		if !c.ok {
			if m != nil {
				t.Errorf("%q: got %q, want no match", c.compat, m[1])
			}
			continue
		}
		if m == nil || m[1] != c.code {
			t.Errorf("%q: got %v, want %q", c.compat, m, c.code)
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
