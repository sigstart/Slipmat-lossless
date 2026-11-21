// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

// Runs outside the FairPlay sandbox. Only this process can access the keyring.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

const maxProfile = 16 << 20

var profileFiles = []string{
	"device-id", "adi.pb", "IC-Info.sids", "fsi.pdat",
	"session-ready", "STOREFRONT_ID", "MUSIC_TOKEN",
	"mpl_db/accounts.sqlitedb", "mpl_db/accounts.sqlitedb-wal", "mpl_db/accounts.sqlitedb-journal",
	"mpl_db/cookies.sqlitedb", "mpl_db/cookies.sqlitedb-wal", "mpl_db/cookies.sqlitedb-journal",
}

func main() {
	if len(os.Args) != 5 || run(os.Args[1], os.Args[2], os.Args[3], os.Args[4]) != nil {
		os.Exit(1)
	}
}

func run(launcher, profile, archive, mode string) error {
	if err := os.Mkdir(profile, 0700); err != nil {
		return err
	}
	defer os.RemoveAll(profile)

	switch mode {
	case "restore":
		if err := load(profile, archive); err != nil {
			_, err := fmt.Fprintln(os.Stdout, `{"type":"failed"}`)
			return err
		}
	case "seed":
	default:
		return errors.New("invalid profile mode")
	}

	child := exec.Command(launcher, profile, mode)
	child.Stdin = os.Stdin
	child.Stderr = os.Stderr
	stdout, err := child.StdoutPipe()
	if err != nil {
		return err
	}
	if err := child.Start(); err != nil {
		return err
	}
	defer child.Process.Kill()

	frames := bufio.NewScanner(stdout)
	frames.Buffer(make([]byte, 8192), 8192)
	for frames.Scan() {
		frame := frames.Bytes()
		var reply struct{ Type string }
		if mode == "seed" && json.Unmarshal(frame, &reply) == nil && reply.Type == "signedIn" {
			// Sign-in can still be used when the desktop keyring is unavailable.
			if err := save(profile, archive); err != nil {
				fmt.Fprintln(os.Stderr, "Slipmat could not save its native session")
			}
		}
		if _, err := fmt.Fprintln(os.Stdout, string(frame)); err != nil {
			return err
		}
	}
	return errors.Join(frames.Err(), child.Wait())
}

func keyring(archive string, create bool) ([]byte, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	attributes := []string{"application", "dev.miguelrincon.Slipmat", "profile", archive}
	output, err := exec.CommandContext(ctx, "secret-tool", append([]string{"lookup"}, attributes...)...).Output()
	defer clear(output)
	if err == nil {
		key, err := hex.DecodeString(string(bytes.TrimSpace(output)))
		if err != nil || len(key) != 32 {
			clear(key)
			return nil, errors.New("invalid profile key")
		}
		return key, nil
	}
	if !create {
		return nil, err
	}
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		return nil, err
	}
	encoded := make([]byte, hex.EncodedLen(len(key)))
	defer clear(encoded)
	hex.Encode(encoded, key)
	store := exec.CommandContext(ctx, "secret-tool", append([]string{"store", "--label=Slipmat native session key"}, attributes...)...)
	store.Stdin = bytes.NewReader(encoded)
	if err := store.Run(); err != nil {
		clear(key)
		return nil, err
	}
	return key, nil
}

func profileCipher(archive string, create bool) (cipher.AEAD, error) {
	key, err := keyring(archive, create)
	if err != nil {
		return nil, err
	}
	defer clear(key)
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, err
	}
	return cipher.NewGCM(block)
}

func readLimited(path string, limit int64) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	stat, err := file.Stat()
	if err != nil || !stat.Mode().IsRegular() || stat.Size() > limit {
		return nil, errors.New("invalid profile file")
	}
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if int64(len(data)) > limit {
		clear(data)
		return nil, errors.New("profile file too large")
	}
	return data, err
}

func snapshot(profile string) ([]byte, error) {
	files := make(map[string][]byte)
	defer func() {
		for _, data := range files {
			clear(data)
		}
	}()
	var size int64
	for _, name := range profileFiles {
		data, err := readLimited(filepath.Join(profile, "files", name), maxProfile-size)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, err
		}
		files[name] = data
		size += int64(len(data))
	}
	if len(files["session-ready"]) == 0 || len(files["mpl_db/accounts.sqlitedb"]) == 0 {
		return nil, errors.New("incomplete native profile")
	}
	return json.Marshal(files)
}

const archiveVersion = "slipmat-native-profile-v1"

func save(profile, archive string) error {
	data, err := snapshot(profile)
	if err != nil {
		return err
	}
	defer clear(data)
	check, err := snapshot(profile)
	defer clear(check)
	if err != nil || !bytes.Equal(data, check) {
		return errors.New("native profile changed during snapshot")
	}
	aead, err := profileCipher(archive, true)
	if err != nil {
		return err
	}
	nonce := make([]byte, aead.NonceSize())
	if _, err := rand.Read(nonce); err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(archive), ".native-session-*")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	defer file.Close()
	if _, err := file.Write(aead.Seal(nonce, nonce, data, []byte(archiveVersion))); err != nil {
		return err
	}
	if err := file.Sync(); err != nil {
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Rename(file.Name(), archive)
}

func load(profile, archive string) error {
	blob, err := readLimited(archive, 2*maxProfile)
	if err != nil {
		return err
	}
	aead, err := profileCipher(archive, false)
	if err != nil {
		return err
	}
	if len(blob) < aead.NonceSize() {
		return errors.New("invalid profile archive")
	}
	data, err := aead.Open(nil, blob[:aead.NonceSize()], blob[aead.NonceSize():], []byte(archiveVersion))
	if err != nil {
		return err
	}
	defer clear(data)
	var files map[string][]byte
	defer func() {
		for _, data := range files {
			clear(data)
		}
	}()
	if err := json.Unmarshal(data, &files); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Join(profile, "files", "mpl_db"), 0700); err != nil {
		return err
	}
	for _, name := range profileFiles {
		if data, exists := files[name]; exists {
			if err := os.WriteFile(filepath.Join(profile, "files", name), data, 0600); err != nil {
				return err
			}
		}
	}
	return nil
}
