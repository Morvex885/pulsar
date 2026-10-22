package main

import (
	"archive/zip"
	"bytes"
	"crypto/sha1"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// Android SDK Platform-Tools: Google's license forbids redistributing them, so they are
// downloaded on first use. Pins from https://dl.google.com/android/repository/repository2-3.xml.
const ptBase = "https://dl.google.com/android/repository/"

type ptPackage struct {
	file, sha1 string
	size       int64
}

// GOOS → package; linux only on amd64 (Google builds no other).
var ptPackages = map[string]ptPackage{
	"windows": {"platform-tools_r37.0.1-win.zip", "e03e78b1d80b396f1c3358e31251cb31740e1110", 8044989},
	"darwin":  {"platform-tools_r37.0.1-darwin.zip", "6ae73f4de6452dc57e62ec02b68eed92a4c21661", 16110554},
	"linux":   {"platform-tools_r37.0.1-linux.zip", "477254aa5f903c15cf51001717bdf347fb6b53e0", 9054187},
}

// fetchPlatformTools downloads url and checks size and sha1. It unpacks the zip's platform-tools/
// tree into dest+".tmp-<pid>" and renames that to dest. On any error nothing is left behind.
func fetchPlatformTools(url string, size int64, sha1hex, dest string) error {
	client := http.Client{Timeout: 5 * time.Minute}
	resp, err := client.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %s", resp.Status)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, size+1))
	if err != nil {
		return err
	}
	sum := sha1.Sum(data)
	if got := hex.EncodeToString(sum[:]); int64(len(data)) != size || got != sha1hex {
		return fmt.Errorf("checksum mismatch (got %s)", got)
	}
	zr, err := zip.NewReader(bytes.NewReader(data), int64(len(data)))
	if err != nil {
		return err
	}
	tmp := fmt.Sprintf("%s.tmp-%d", dest, os.Getpid())
	if err := os.RemoveAll(tmp); err != nil {
		return err
	}
	if err := unpack(zr, tmp); err != nil {
		os.RemoveAll(tmp)
		return err
	}
	if err := os.Rename(tmp, dest); err != nil {
		os.RemoveAll(tmp)
		if _, serr := os.Stat(dest); serr == nil {
			return nil // another run got there first
		}
		return err
	}
	return nil
}

func unpack(zr *zip.Reader, dir string) error {
	for _, f := range zr.File {
		rest, ok := strings.CutPrefix(f.Name, "platform-tools/")
		if !ok || (rest != "" && !filepath.IsLocal(rest)) {
			return fmt.Errorf("unexpected entry %s", f.Name)
		}
		p := filepath.Join(dir, filepath.FromSlash(rest))
		mode := f.Mode()
		switch {
		case mode.IsDir():
			if err := os.MkdirAll(p, 0o755); err != nil {
				return err
			}
		case mode&fs.ModeSymlink != 0:
			target, err := readEntry(f)
			if err != nil {
				return err
			}
			if !filepath.IsLocal(filepath.Join(filepath.Dir(rest), target)) || filepath.IsAbs(target) {
				return fmt.Errorf("unexpected entry %s -> %s", f.Name, target)
			}
			if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
				return err
			}
			if err := os.Symlink(target, p); err != nil {
				return err
			}
		case mode.IsRegular():
			if rest == "" {
				return fmt.Errorf("unexpected entry %s", f.Name)
			}
			if err := writeEntry(f, p, mode.Perm()|0o600); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unexpected entry %s", f.Name)
		}
	}
	return nil
}

func readEntry(f *zip.File) (string, error) {
	r, err := f.Open()
	if err != nil {
		return "", err
	}
	defer r.Close()
	b, err := io.ReadAll(io.LimitReader(r, 4096))
	return string(b), err
}

func writeEntry(f *zip.File, p string, perm fs.FileMode) error {
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	r, err := f.Open()
	if err != nil {
		return err
	}
	defer r.Close()
	w, err := os.OpenFile(p, os.O_WRONLY|os.O_CREATE|os.O_EXCL, perm)
	if err != nil {
		return err
	}
	_, err = io.Copy(w, r)
	return errors.Join(err, w.Close())
}
