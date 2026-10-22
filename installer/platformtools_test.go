package main

import (
	"archive/zip"
	"bytes"
	"crypto/sha1"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func zipOf(t *testing.T, name string) []byte {
	t.Helper()
	var buf bytes.Buffer
	zw := zip.NewWriter(&buf)
	h := &zip.FileHeader{Name: name, Method: zip.Deflate}
	h.SetMode(0o755)
	w, err := zw.CreateHeader(h)
	if err != nil {
		t.Fatal(err)
	}
	w.Write([]byte("x"))
	if err := zw.Close(); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func serve(t *testing.T, body []byte) (url, sum string) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write(body) }))
	t.Cleanup(srv.Close)
	s := sha1.Sum(body)
	return srv.URL, hex.EncodeToString(s[:])
}

// leftovers lists everything in dir: dest and tmp dirs are its only possible entries.
func leftovers(t *testing.T, dir string) []string {
	t.Helper()
	m, err := filepath.Glob(filepath.Join(dir, "*"))
	if err != nil {
		t.Fatal(err)
	}
	return m
}

func TestFetchPlatformTools(t *testing.T) {
	body := zipOf(t, "platform-tools/adb")
	url, sum := serve(t, body)
	dir := t.TempDir()
	dest := filepath.Join(dir, "platform-tools")
	if err := fetchPlatformTools(url, int64(len(body)), sum, dest); err != nil {
		t.Fatal(err)
	}
	st, err := os.Stat(filepath.Join(dest, "adb"))
	if err != nil {
		t.Fatal(err)
	}
	if runtime.GOOS != "windows" && st.Mode().Perm()&0o100 == 0 {
		t.Errorf("adb not executable: %v", st.Mode())
	}
	if l := leftovers(t, dir); len(l) != 1 {
		t.Errorf("leftovers: %v", l)
	}
}

func TestFetchPlatformToolsBadChecksum(t *testing.T) {
	body := zipOf(t, "platform-tools/adb")
	url, _ := serve(t, body)
	dir := t.TempDir()
	if err := fetchPlatformTools(url, int64(len(body)), "0000000000000000000000000000000000000000", filepath.Join(dir, "platform-tools")); err == nil {
		t.Error("wrong sha1 accepted")
	}
	if l := leftovers(t, dir); len(l) != 0 {
		t.Errorf("leftovers: %v", l)
	}
}

func TestFetchPlatformToolsEscape(t *testing.T) {
	body := zipOf(t, "platform-tools/../evil")
	url, sum := serve(t, body)
	dir := t.TempDir()
	if err := fetchPlatformTools(url, int64(len(body)), sum, filepath.Join(dir, "sub", "platform-tools")); err == nil {
		t.Error("entry outside platform-tools/ accepted")
	}
	if l := leftovers(t, dir); len(l) != 0 {
		t.Errorf("leftovers: %v", l)
	}
}
