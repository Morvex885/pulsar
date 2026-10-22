package main

import (
	"fmt"
	"net"
	"net/netip"
	"os"
	"slices"
	"strings"
	"sync"
	"time"

	"golang.org/x/net/dns/dnsmessage"
	"golang.org/x/net/ipv4"
)

// stationAnnounce reports whether an mDNS response announces a Yandex Station: an answer PTR for
// _yandexio._tcp.local. (stock firmware), or for _esphomelib._tcp.local. whose instance label
// starts with "yandex-" (pulsar). Names compared case-insensitively; unparsable packets → false.
func stationAnnounce(pkt []byte) bool {
	var p dnsmessage.Parser
	h, err := p.Start(pkt)
	if err != nil || !h.Response || p.SkipAllQuestions() != nil {
		return false
	}
	for {
		ah, err := p.AnswerHeader()
		if err != nil {
			return false
		}
		if ah.Type != dnsmessage.TypePTR {
			if p.SkipAnswer() != nil {
				return false
			}
			continue
		}
		r, err := p.PTRResource()
		if err != nil {
			return false
		}
		switch strings.ToLower(ah.Name.String()) {
		case "_yandexio._tcp.local.":
			return true
		case "_esphomelib._tcp.local.":
			if strings.HasPrefix(strings.ToLower(r.PTR.String()), "yandex-") {
				return true
			}
		}
	}
}

// defaultIPv4 is the local address of the default-route interface (no packet is sent).
func defaultIPv4() (net.IP, error) {
	c, err := net.Dial("udp4", "192.0.2.1:9")
	if err != nil {
		return nil, err
	}
	defer c.Close()
	ip := c.LocalAddr().(*net.UDPAddr).IP.To4()
	if ip == nil || ip.IsLoopback() {
		return nil, fmt.Errorf("no IPv4 default route")
	}
	return ip, nil
}

func localIPv4() net.IP {
	ip, err := defaultIPv4()
	if err != nil {
		fmt.Fprintln(os.Stderr, "no default-route IPv4 interface found: pass the station's address (pulsar-install <ip>)")
		return nil
	}
	return ip
}

func sortAddrs(a []string) {
	slices.SortFunc(a, func(x, y string) int {
		return netip.MustParseAddrPort(x).Compare(netip.MustParseAddrPort(y))
	})
}

// scanNet returns ip:port for the hosts prefix.1 … prefix.254 accepting TCP on port.
func scanNet(prefix string, port int) []string {
	var (
		mu    sync.Mutex
		found []string
		wg    sync.WaitGroup
		sem   = make(chan struct{}, 64)
	)
	for i := 1; i <= 254; i++ {
		addr := fmt.Sprintf("%s.%d:%d", prefix, i, port)
		wg.Add(1)
		sem <- struct{}{}
		go func() {
			defer func() { <-sem; wg.Done() }()
			c, err := net.DialTimeout("tcp4", addr, 500*time.Millisecond)
			if err != nil {
				return
			}
			c.Close()
			mu.Lock()
			found = append(found, addr)
			mu.Unlock()
		}()
	}
	wg.Wait()
	sortAddrs(found)
	return found
}

// scan finds open adb ports in the /24 of the default-route interface. It may reach other Android
// devices, which can show an adb authorization prompt.
func scan() []string {
	ip := localIPv4()
	if ip == nil {
		return nil
	}
	prefix := fmt.Sprintf("%d.%d.%d", ip[0], ip[1], ip[2])
	fmt.Fprintf(os.Stderr, "scanning %s.0/24 for adb...\n", prefix)
	return scanNet(prefix, 5555)
}

func mdnsQuery(service string) []byte {
	b := dnsmessage.NewBuilder(nil, dnsmessage.Header{})
	b.StartQuestions()
	b.Question(dnsmessage.Question{Name: dnsmessage.MustNewName(service), Type: dnsmessage.TypePTR, Class: dnsmessage.ClassINET})
	pkt, _ := b.Finish()
	return pkt
}

// mdns returns ip:5555 for Yandex Stations announced over mDNS: stock firmware as _yandexio._tcp,
// converted ones as ESPHome devices named yandex-*. Only these get an `adb connect`, so other
// Android devices on the network are left alone. Queries are sent from an ephemeral port (mDNS
// "legacy unicast"): responders answer to that port directly, so port 5353 need not be free.
func mdns() []string {
	fmt.Fprintln(os.Stderr, "looking for stations via mDNS...")
	local := localIPv4()
	if local == nil {
		return nil
	}
	c, err := net.ListenUDP("udp4", &net.UDPAddr{IP: local})
	if err != nil {
		fmt.Fprintf(os.Stderr, "mDNS: %v\n", err)
		return nil
	}
	defer c.Close()
	pc := ipv4.NewPacketConn(c)
	if ifs, err := net.Interfaces(); err == nil {
	find:
		for _, ifi := range ifs {
			addrs, _ := ifi.Addrs()
			for _, a := range addrs {
				if n, ok := a.(*net.IPNet); ok && n.IP.Equal(local) {
					_ = pc.SetMulticastInterface(&ifi)
					break find
				}
			}
		}
	}
	_ = pc.SetMulticastTTL(255)
	group := &net.UDPAddr{IP: net.IPv4(224, 0, 0, 251), Port: 5353}
	queries := [][]byte{mdnsQuery("_yandexio._tcp.local."), mdnsQuery("_esphomelib._tcp.local.")}
	send := func() {
		for _, q := range queries {
			_, _ = c.WriteToUDP(q, group)
		}
	}
	send()
	start := time.Now()
	resent := false
	seen := map[string]bool{}
	var found []string
	buf := make([]byte, 9000)
	for {
		deadline := start.Add(3 * time.Second)
		if !resent {
			deadline = start.Add(time.Second)
		}
		_ = c.SetReadDeadline(deadline)
		n, from, err := c.ReadFromUDP(buf)
		if err != nil {
			if ne, ok := err.(net.Error); !ok || !ne.Timeout() || resent {
				break
			}
			send()
			resent = true
			continue
		}
		if ip := from.IP.To4(); ip != nil && stationAnnounce(buf[:n]) {
			a := ip.String() + ":5555"
			if !seen[a] {
				seen[a] = true
				found = append(found, a)
			}
		}
	}
	sortAddrs(found)
	return found
}
