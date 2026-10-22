package main

import (
	"testing"

	"golang.org/x/net/dns/dnsmessage"
)

func ptrResponse(t *testing.T, service, instance string) []byte {
	t.Helper()
	b := dnsmessage.NewBuilder(nil, dnsmessage.Header{Response: true, Authoritative: true})
	if err := b.StartAnswers(); err != nil {
		t.Fatal(err)
	}
	h := dnsmessage.ResourceHeader{Name: dnsmessage.MustNewName(service), Class: dnsmessage.ClassINET, TTL: 120}
	if err := b.PTRResource(h, dnsmessage.PTRResource{PTR: dnsmessage.MustNewName(instance)}); err != nil {
		t.Fatal(err)
	}
	pkt, err := b.Finish()
	if err != nil {
		t.Fatal(err)
	}
	return pkt
}

func TestStationAnnounce(t *testing.T) {
	cases := []struct {
		pkt  []byte
		want bool
	}{
		{ptrResponse(t, "_esphomelib._tcp.local.", "yandex-midi-a1b2c3._esphomelib._tcp.local."), true},
		{ptrResponse(t, "_yandexio._tcp.local.", "YandexIOReceiver-abc._yandexio._tcp.local."), true},
		{ptrResponse(t, "_esphomelib._tcp.local.", "kitchen._esphomelib._tcp.local."), false},
		{[]byte{1, 2, 3}, false},
	}
	for i, c := range cases {
		if got := stationAnnounce(c.pkt); got != c.want {
			t.Errorf("case %d: got %v, want %v", i+1, got, c.want)
		}
	}
}
