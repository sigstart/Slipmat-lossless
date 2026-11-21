// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

// Built against the pinned reference engine by prepare.sh.
package main

/*
#include <stdlib.h>
char *drm_lib_get_account(void);
*/
import "C"

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unsafe"

	"engine/core/drm"
	"engine/core/fairplay"
	"engine/core/hls"
	"engine/core/pipeline"
)

type request struct {
	Type     string `json:"type"`
	Account  string `json:"account"`
	Password string `json:"password"`
	Code     string `json:"code"`
	TrackID  string `json:"track_id"`
}

type result struct {
	Type          string  `json:"type"`
	Stage         string  `json:"stage,omitempty"`
	SampleRate    uint32  `json:"sample_rate,omitempty"`
	BitsPerSample uint8   `json:"bits_per_sample,omitempty"`
	Channels      uint8   `json:"channels,omitempty"`
	PositionMS    *uint64 `json:"position_ms,omitempty"`
}

type wire struct {
	input  *bufio.Scanner
	output *json.Encoder
}

func main() {
	os.Exit(run())
}

func run() int {
	// Native libraries can write to stdout. Reserve the inherited pipe for
	// typed replies and discard the native and Go trace streams.
	fd, err := syscall.Dup(int(os.Stdout.Fd()))
	if err != nil {
		return 1
	}
	syscall.CloseOnExec(fd)
	control := os.NewFile(uintptr(fd), "control")
	defer control.Close()
	null, err := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
	if err != nil {
		return 1
	}
	defer null.Close()
	if syscall.Dup2(int(null.Fd()), int(os.Stdout.Fd())) != nil {
		return 1
	}
	log.SetOutput(io.Discard)
	input := bufio.NewScanner(os.Stdin)
	input.Buffer(make([]byte, 8192), 8192)
	stream := &wire{input: input, output: json.NewEncoder(control)}
	if !stream.signInAndPlay() {
		return 1
	}
	return 0
}

func (w *wire) read(kind string) (request, bool) {
	msg, ok := w.readRequest()
	return msg, ok && msg.Type == kind
}

func (w *wire) readRequest() (request, bool) {
	var msg request
	if !w.input.Scan() {
		return msg, false
	}
	line := w.input.Bytes()
	err := json.Unmarshal(line, &msg)
	clear(line)
	return msg, err == nil
}

func (w *wire) Challenge(_ context.Context, challenge drm.AuthChallenge) (string, error) {
	if challenge.Type != drm.ChallengeTwoFactor {
		// The pinned bridge can choose a dialog option when its callback
		// returns an error. Exit before returning to that bridge.
		_ = w.output.Encode(result{Type: "verificationUnsupported"})
		os.Exit(1)
	}
	if err := w.output.Encode(result{Type: "verificationRequired"}); err != nil {
		return "", err
	}
	msg, ok := w.read("verificationCode")
	if !ok || len(msg.Code) != 6 || strings.IndexFunc(msg.Code, nonDigit) >= 0 {
		return "", errors.New("verification cancelled")
	}
	return msg.Code, nil
}

func (w *wire) signInAndPlay() bool {
	if _, ok := w.read("begin"); !ok {
		return false
	}
	if w.output.Encode(result{Type: "credentialsRequired"}) != nil {
		return false
	}
	credentials, ok := w.read("credentials")
	if !ok {
		return false
	}
	if credentials.Account == "" || credentials.Password == "" ||
		strings.ContainsRune(credentials.Account, 0) || strings.ContainsRune(credentials.Password, 0) {
		_ = w.output.Encode(result{Type: "rejected"})
		return false
	}
	// The launcher mounts the wrapper's entire writable session profile on
	// private tmpfs and gives it no pre-existing account session.
	backend := drm.AdamplayBackend("/app")
	backend.SetAuthSource(w)
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	cfg := drm.BackendConfig{
		BaseDir: "/state/files",
		Credentials: drm.Credentials{
			Email: credentials.Account, Password: credentials.Password,
		},
	}
	err := backend.Start(ctx, cfg)
	cancel()
	if err != nil {
		_ = w.output.Encode(result{Type: "failed"})
		return false
	}
	defer backend.Stop()
	if w.output.Encode(result{Type: "signedIn"}) != nil {
		return false
	}
	return w.playTracks(func(ctx context.Context, track string, control *playbackControl, report func(result)) result {
		return play(ctx, backend, track, control, report)
	})
}

type trackPlayer func(context.Context, string, *playbackControl, func(result)) result

func (w *wire) playTracks(playTrack trackPlayer) bool {
	inputCtx, closeInput := context.WithCancel(context.Background())
	defer closeInput()
	commands := make(chan request)
	go func() {
		defer close(commands)
		for {
			cmd, ok := w.readRequest()
			if !ok {
				return
			}
			select {
			case commands <- cmd:
			case <-inputCtx.Done():
				return
			}
		}
	}()

	var updates <-chan result
	var done <-chan result
	var cancel context.CancelFunc
	var control *playbackControl
	stopping := false
	defer func() {
		if done != nil {
			cancel()
			<-done
		}
	}()
	for {
		select {
		case cmd, ok := <-commands:
			if !ok {
				return true
			}
			switch cmd.Type {
			case "playTrack":
				if done != nil {
					continue
				}
				ctx, cancelTrack := context.WithCancel(inputCtx)
				cancel = cancelTrack
				trackControl := &playbackControl{changed: make(chan struct{}, 1)}
				control = trackControl
				trackUpdates, trackDone := make(chan result), make(chan result, 1)
				updates, done = trackUpdates, trackDone
				go func() {
					trackDone <- playTrack(ctx, cmd.TrackID, trackControl, func(event result) {
						select {
						case trackUpdates <- event:
						case <-ctx.Done():
						}
					})
				}()
			case "pause", "resume":
				if done != nil && !stopping {
					control.setPaused(cmd.Type == "pause")
				}
			case "stop":
				if done != nil {
					stopping = true
					cancel()
				}
			}
		case event := <-updates:
			if !stopping && w.output.Encode(event) != nil {
				return false
			}
		case terminal := <-done:
			cancel()
			updates, done = nil, nil
			if stopping {
				terminal = result{Type: "stopped"}
				stopping = false
			}
			if w.output.Encode(terminal) != nil {
				return false
			}
		}
	}
}

func play(ctx context.Context, backend drm.DRMBackend, track string, control *playbackControl, report func(result)) result {
	if num, err := strconv.ParseUint(track, 10, 64); err != nil || num == 0 ||
		strings.IndexFunc(track, nonDigit) >= 0 {
		return result{Type: "trialFailed", Stage: "catalogue"}
	}
	account, err := nativeAccount()
	if err != nil {
		return result{Type: "trialFailed", Stage: "session"}
	}
	masterURL, failure := catalogueALAC(ctx, musicClient, account, track)
	if failure != "" {
		return result{Type: "trialFailed", Stage: failure}
	}
	master, err := hls.OpenMaster(ctx, masterURL)
	if err != nil {
		return result{Type: "trialFailed", Stage: "stream"}
	}
	mediaURL := selectALAC(master)
	if !httpsURL(mediaURL) {
		return result{Type: "trialFailed", Stage: "stream"}
	}
	media, err := hls.OpenMediaCBCS(ctx, mediaURL)
	if err != nil || !httpsURL(media.FileURL) ||
		!strings.HasPrefix(fairplay.FirstAudioKeyURI(media.KeyURIs), "skd://") {
		return result{Type: "trialFailed", Stage: "stream"}
	}
	source := fairplay.CBCSSeekableSource(track, backend, media, 0)
	return output(ctx, source, "/app/fairplay-alac-output", control, report)
}

func nativeAccount() (drm.AccountInfo, error) {
	raw := C.drm_lib_get_account()
	if raw == nil {
		return drm.AccountInfo{}, errors.New("native account unavailable")
	}
	defer C.free(unsafe.Pointer(raw))
	var account drm.AccountInfo
	if err := json.Unmarshal([]byte(C.GoString(raw)), &account); err != nil ||
		account.MusicToken == "" || account.DevToken == "" {
		return drm.AccountInfo{}, errors.New("native account authorization unavailable")
	}
	return account, nil
}

const musicAPI = "https://api.music.apple.com/v1"
const assetCatalogue = "https://amp-api.music.apple.com/v1"

var musicClient = &http.Client{
	Timeout: 20 * time.Second,
	CheckRedirect: func(*http.Request, []*http.Request) error {
		return http.ErrUseLastResponse
	},
}

func catalogueALAC(ctx context.Context, client *http.Client, account drm.AccountInfo, track string) (string, string) {
	storefront := strings.ToLower(account.StorefrontID)
	if !storefrontCode(storefront) {
		var regions struct {
			Data []struct{ ID string }
		}
		switch musicJSON(ctx, client, musicAPI+"/me/storefront", account.DevToken, account.MusicToken, &regions) {
		case http.StatusOK:
			if len(regions.Data) != 1 || !storefrontCode(regions.Data[0].ID) {
				return "", "storefront"
			}
			storefront = regions.Data[0].ID
		case http.StatusUnauthorized, http.StatusForbidden:
			return "", "catalogueAuthorization"
		default:
			return "", "storefront"
		}
	}
	var song struct {
		Data []struct {
			ID         string
			Attributes struct {
				ExtendedAssetUrls struct {
					EnhancedHls string
				}
			}
		}
	}
	endpoint := assetCatalogue + "/catalog/" + storefront + "/songs/" + track + "?extend=extendedAssetUrls"
	switch musicJSON(ctx, client, endpoint, account.DevToken, "", &song) {
	case http.StatusOK:
		if len(song.Data) != 1 || song.Data[0].ID != track {
			return "", "catalogueTrack"
		}
	case http.StatusUnauthorized, http.StatusForbidden:
		return "", "catalogueAuthorization"
	case http.StatusNotFound:
		return "", "catalogueTrack"
	default:
		return "", "catalogue"
	}
	asset := song.Data[0].Attributes.ExtendedAssetUrls.EnhancedHls
	if !httpsURL(asset) {
		return "", "catalogueAsset"
	}
	return asset, ""
}

func storefrontCode(value string) bool {
	return len(value) == 2 && value[0] >= 'a' && value[0] <= 'z' &&
		value[1] >= 'a' && value[1] <= 'z'
}

func musicJSON(ctx context.Context, client *http.Client, endpoint, token, userToken string, data any) int {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return 0
	}
	req.Header.Set("Authorization", "Bearer "+token)
	if req.URL.Host == "amp-api.music.apple.com" {
		req.Header.Set("Origin", "https://music.apple.com")
	}
	if userToken != "" {
		req.Header.Set("Music-User-Token", userToken)
	}
	res, err := client.Do(req)
	if err != nil {
		return 0
	}
	defer res.Body.Close()
	if res.StatusCode == http.StatusOK && json.NewDecoder(io.LimitReader(res.Body, 1<<20)).Decode(data) != nil {
		return 0
	}
	return res.StatusCode
}

const maxTrialBytes = 256 << 20

type trialBuffer struct{ data bytes.Buffer }

func (b *trialBuffer) Write(data []byte) (int, error) {
	if len(data) > maxTrialBytes-b.data.Len() {
		return 0, errors.New("trial track exceeds the in-memory limit")
	}
	return b.data.Write(data)
}

func output(ctx context.Context, source pipeline.Source, decoder string, control *playbackControl, report func(result)) result {
	// The pinned CBCS API reports a failed licence as a stream error and can
	// deliver a shared prefetch fragment first. Require the complete trial
	// track before opening audio so an ambiguous failure produces silence.
	var media trialBuffer
	err := source.Stream(ctx, &media)
	defer clear(media.data.Bytes())
	if err != nil || ctx.Err() != nil || media.data.Len() == 0 {
		return result{Type: "trialFailed", Stage: "stream"}
	}
	played, err := runDecoder(ctx, decoder, media.data.Bytes(), control, report)
	if err != nil {
		return result{Type: "trialFailed", Stage: "output"}
	}
	return played
}

type playbackControl struct {
	mu      sync.Mutex
	paused  bool
	changed chan struct{}
}

func (c *playbackControl) setPaused(paused bool) {
	c.mu.Lock()
	c.paused = paused
	c.mu.Unlock()
	select {
	case c.changed <- struct{}{}:
	default:
	}
}

func (c *playbackControl) start(cmd *exec.Cmd) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	// The startup pause must be readable before the decoder sees any media.
	if c.paused {
		cmd.Args = append(cmd.Args, "--paused")
	}
	return c.paused, cmd.Start()
}

func (c *playbackControl) forward(ctx context.Context, writer *os.File, paused bool) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-c.changed:
		}
		c.mu.Lock()
		wanted := c.paused
		c.mu.Unlock()
		if wanted == paused {
			continue
		}
		command := "resume\n"
		if wanted {
			command = "pause\n"
		}
		if _, err := writer.WriteString(command); err != nil {
			return
		}
		paused = wanted
	}
}

func runDecoder(ctx context.Context, decoder string, media []byte, control *playbackControl, report func(result)) (result, error) {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	cmd := exec.CommandContext(ctx, decoder, "/audio/native", "--controlled")
	cmd.SysProcAttr = &syscall.SysProcAttr{Pdeathsig: syscall.SIGKILL}
	cmd.Stdin = bytes.NewReader(media)
	reader, writer, err := os.Pipe()
	if err != nil {
		return result{}, err
	}
	defer reader.Close()
	defer writer.Close()
	cmd.ExtraFiles = []*os.File{reader}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return result{}, err
	}
	defer stdout.Close()
	paused, err := control.start(cmd)
	if err != nil {
		return result{}, err
	}
	_ = reader.Close()
	controlsDone := make(chan struct{})
	go func() {
		defer close(controlsDone)
		control.forward(ctx, writer, paused)
	}()
	played, err := readDecoder(stdout, report)
	if err != nil {
		cancel()
	}
	waitErr := cmd.Wait()
	cancel()
	_ = writer.Close()
	<-controlsDone
	return played, errors.Join(err, waitErr)
}

func readDecoder(stdout io.Reader, report func(result)) (result, error) {
	var played result
	scanner := bufio.NewScanner(stdout)
	for scanner.Scan() {
		var msg result
		if json.Unmarshal(scanner.Bytes(), &msg) != nil || played.Type != "" {
			return result{}, errors.New("invalid decoder reply")
		}
		switch msg.Type {
		case "playing", "paused":
			if msg.PositionMS != nil {
				report(result{Type: "position", PositionMS: msg.PositionMS})
			}
			report(result{Type: msg.Type})
		case "position":
			if msg.PositionMS == nil {
				return result{}, errors.New("missing decoder position")
			}
			report(result{Type: msg.Type, PositionMS: msg.PositionMS})
		case "":
			if msg.SampleRate == 0 || msg.BitsPerSample == 0 || msg.Channels == 0 {
				return result{}, errors.New("missing decoder format")
			}
			played = result{Type: "played", SampleRate: msg.SampleRate, BitsPerSample: msg.BitsPerSample, Channels: msg.Channels}
		default:
			return result{}, errors.New("unknown decoder reply")
		}
	}
	if err := scanner.Err(); err != nil {
		return result{}, err
	}
	if played.Type == "" {
		return result{}, errors.New("missing decoder terminal reply")
	}
	return played, nil
}

func httpsURL(value string) bool {
	uri, err := url.Parse(value)
	return err == nil && uri.Scheme == "https" && uri.Hostname() != "" && uri.User == nil
}

func selectALAC(master *hls.Master) string {
	var best hls.Variant
	for _, variant := range master.Variants {
		for _, codec := range strings.Split(variant.Codecs, ",") {
			if strings.EqualFold(strings.TrimSpace(codec), "alac") &&
				(best.URL == "" || variant.Bandwidth > best.Bandwidth) {
				best = variant
			}
		}
	}
	return best.URL
}

func nonDigit(r rune) bool { return r < '0' || r > '9' }
