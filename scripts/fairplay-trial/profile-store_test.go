// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

package main

import (
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
)

func syntheticProfile(t *testing.T) *os.File {
	t.Helper()
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "files", "mpl_db"), 0700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"session-ready", "mpl_db/accounts.sqlitedb"} {
		if err := os.WriteFile(filepath.Join(root, "files", name), []byte(name), 0600); err != nil {
			t.Fatal(err)
		}
	}
	profile, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { profile.Close() })
	return profile
}

func TestProfileArchiveRoundTrip(t *testing.T) {
	profile := syntheticProfile(t)
	bin := t.TempDir()
	t.Setenv("PATH", bin)
	keyring := "#!/bin/sh\nprintf '%s\\n' " + strings.Repeat("ab", 32) + "\n"
	if err := os.WriteFile(filepath.Join(bin, "secret-tool"), []byte(keyring), 0700); err != nil {
		t.Fatal(err)
	}

	archive := filepath.Join(t.TempDir(), "native-session.enc")
	if err := save(profile, archive); err != nil {
		t.Fatal(err)
	}
	restored := t.TempDir()
	if err := load(restored, archive); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"session-ready", "mpl_db/accounts.sqlitedb"} {
		data, err := os.ReadFile(filepath.Join(restored, "files", name))
		if err != nil || string(data) != name {
			t.Fatalf("restoring %s: got %q, error %v", name, data, err)
		}
	}
}

func TestSnapshotRejectsSymlinks(t *testing.T) {
	for _, name := range []string{"files", "files/mpl_db", "files/session-ready"} {
		t.Run(name, func(t *testing.T) {
			profile := syntheticProfile(t)
			entry := filepath.Join(profile.Name(), name)
			original := entry + ".original"
			if err := os.Rename(entry, original); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(filepath.Base(original), entry); err != nil {
				t.Fatal(err)
			}
			if _, err := snapshot(profile); err == nil {
				t.Fatal("snapshot accepted a symlink")
			}
		})
	}
}

func TestSnapshotRejectsFIFO(t *testing.T) {
	profile := syntheticProfile(t)
	if err := syscall.Mkfifo(filepath.Join(profile.Name(), "files", "device-id"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := snapshot(profile); err == nil {
		t.Fatal("snapshot accepted a FIFO")
	}
}
