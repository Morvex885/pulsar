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
