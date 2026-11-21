// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"

	"engine/core/drm"
	"engine/core/hls"
)

func TestSelectOnlyExplicitALAC(t *testing.T) {
	master := &hls.Master{Variants: []hls.Variant{
		{URL: "https://example.invalid/aac", Codecs: "mp4a.40.2", Bandwidth: 9000},
		{URL: "https://example.invalid/alac", Codecs: "ALAC", Bandwidth: 1000},
	}}
	if got := selectALAC(master); got != master.Variants[1].URL {
		t.Fatalf("selected %q", got)
	}
	master.Variants = master.Variants[:1]
	if got := selectALAC(master); got != "" {
		t.Fatalf("selected fallback %q", got)
	}
}

type refusedSource struct{}

func (refusedSource) Stream(_ context.Context, w io.Writer) error {
	if _, err := io.WriteString(w, "fixture prefetch fragment"); err != nil {
		return err
	}
	return errors.New("synthetic licence refusal")
}

func TestFailedStreamDoesNotReachOutput(t *testing.T) {
	got := output(context.Background(), refusedSource{}, "", nil, func(event result) {
		t.Fatalf("unexpected event: %+v", event)
	})
	if got != (result{Type: "trialFailed", Stage: "stream"}) {
		t.Fatalf("unexpected result: %+v", got)
	}
}

type trackSource func(context.Context, io.Writer) error

func (s trackSource) Stream(ctx context.Context, w io.Writer) error { return s(ctx, w) }

func TestPlaybackControlsAndReuse(t *testing.T) {
	decoder := filepath.Join(t.TempDir(), "decoder")
	if err := os.WriteFile(decoder, []byte(`#!/bin/sh
set -eu
test "$1" = /audio/native
test "$2" = --controlled
media=$(cat)
if [ "$media" = second ]; then
    test "$#" -eq 2
    printf '%s\n' '{"type":"playing"}' '{"sample_rate":44100,"bits_per_sample":16,"channels":2}'
    exit 0
fi
test "$media" = first
test "$#" -eq 3
test "$3" = --paused
printf '%s\n' "$$" > "$0.pid"
printf '%s\n' '{"type":"paused"}'
IFS= read -r command <&3
test "$command" = resume
printf '%s\n' '{"type":"playing"}' '{"type":"position","position_ms":0}' '{"type":"position","position_ms":4294967296}'
IFS= read -r command <&3
test "$command" = pause
printf '%s\n' '{"type":"paused"}'
IFS= read -r command <&3
exit 1
`), 0700); err != nil {
		t.Fatal(err)
	}
	send, receive := playbackSession(t, func(ctx context.Context, track string, control *playbackControl, report func(result)) result {
		source := trackSource(func(ctx context.Context, w io.Writer) error {
			if track == "first" {
				select {
				case <-control.changed:
				case <-ctx.Done():
					return ctx.Err()
				}
			}
			_, err := io.WriteString(w, track)
			return err
		})
		return output(ctx, source, decoder, control, report)
	})
	send(request{Type: "playTrack", TrackID: "first"})
	send(request{Type: "pause"})
	receive(result{Type: "paused"})
	rawPID, err := os.ReadFile(decoder + ".pid")
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(rawPID)))
	if err != nil || pid <= 0 {
		t.Fatalf("invalid decoder PID %q", rawPID)
	}
	send(request{Type: "resume"})
	receive(result{Type: "playing"})
	zero, position := uint64(0), uint64(1<<32)
	receive(result{Type: "position", PositionMS: &zero})
	receive(result{Type: "position", PositionMS: &position})
	send(request{Type: "pause"})
	receive(result{Type: "paused"})
	send(request{Type: "stop"})
	receive(result{Type: "stopped"})
	if err := syscall.Kill(pid, 0); !errors.Is(err, syscall.ESRCH) {
		t.Fatalf("decoder still present after stopped: %v", err)
	}

	send(request{Type: "playTrack", TrackID: "second"})
	receive(result{Type: "playing"})
	receive(result{Type: "played", SampleRate: 44100, BitsPerSample: 16, Channels: 2})

	// A skip can race with the terminal reply. An idle stop must not send
	// an acknowledgement that could be mistaken for the next track ending.
	send(request{Type: "stop"})
	send(request{Type: "playTrack", TrackID: "second"})
	receive(result{Type: "playing"})
	receive(result{Type: "played", SampleRate: 44100, BitsPerSample: 16, Channels: 2})
}

func TestStopCancelsBuffering(t *testing.T) {
	cancelled := make(chan struct{})
	send, receive := playbackSession(t, func(ctx context.Context, _ string, control *playbackControl, report func(result)) result {
		source := trackSource(func(ctx context.Context, w io.Writer) error {
			if _, err := io.WriteString(w, "partial media"); err != nil {
				return err
			}
			<-ctx.Done()
			close(cancelled)
			return ctx.Err()
		})
		return output(ctx, source, "", control, report)
	})
	send(request{Type: "playTrack", TrackID: "123"})
	send(request{Type: "stop"})
	receive(result{Type: "stopped"})
	select {
	case <-cancelled:
	default:
		t.Fatal("stopped before buffering was cancelled")
	}
}

func playbackSession(t *testing.T, play trackPlayer) (func(request), func(result)) {
	t.Helper()
	input, commands := io.Pipe()
	replies, output := io.Pipe()
	w := wire{input: bufio.NewScanner(input), output: json.NewEncoder(output)}
	done := make(chan bool, 1)
	go func() {
		defer output.Close()
		done <- w.playTracks(play)
	}()
	t.Cleanup(func() {
		commands.Close()
		replies.Close()
		if !<-done && !t.Failed() {
			t.Error("playback session failed")
		}
		input.Close()
	})
	encoder := json.NewEncoder(commands)
	decoder := bufio.NewScanner(replies)
	return func(cmd request) {
			t.Helper()
			if err := encoder.Encode(cmd); err != nil {
				t.Fatal(err)
			}
		}, func(want result) {
			t.Helper()
			expected, err := json.Marshal(want)
			if err != nil {
				t.Fatal(err)
			}
			if !decoder.Scan() || decoder.Text() != string(expected) {
				t.Fatalf("got %s, want %s (read error %v)", decoder.Text(), expected, decoder.Err())
			}
		}
}

type roundTrip func(*http.Request) (*http.Response, error)

func (fn roundTrip) RoundTrip(req *http.Request) (*http.Response, error) { return fn(req) }

func TestCatalogueResolvesNativeStorefront(t *testing.T) {
	account := drm.AccountInfo{StorefrontID: "143472-2,31", DevToken: "fixture-dev", MusicToken: "fixture-user"}
	calls := 0
	client := &http.Client{Transport: roundTrip(func(req *http.Request) (*http.Response, error) {
		calls++
		if req.Header.Get("Authorization") != "Bearer fixture-dev" {
			t.Fatalf("wrong catalogue request")
		}
		switch calls {
		case 1:
			if req.URL.Host != "api.music.apple.com" || req.URL.Path != "/v1/me/storefront" ||
				req.Header.Get("Music-User-Token") != "fixture-user" {
				t.Fatalf("wrong storefront request")
			}
			return response(200, `{"data":[{"id":"za"}]}`), nil
		case 2:
			if req.URL.Host != "amp-api.music.apple.com" || req.URL.Path != "/v1/catalog/za/songs/1234" ||
				req.URL.Query().Get("extend") != "extendedAssetUrls" || req.Header.Get("Music-User-Token") != "" {
				t.Fatalf("wrong song request")
			}
			return response(200, `{"data":[{"id":"1234","attributes":{"extendedAssetUrls":{"enhancedHls":"https://example.invalid/master.m3u8"}}}]}`), nil
		default:
			t.Fatalf("unexpected request")
			return nil, nil
		}
	})}
	if asset, failure := catalogueALAC(context.Background(), client, account, "1234"); failure != "" ||
		asset != "https://example.invalid/master.m3u8" || calls != 2 {
		t.Fatalf("got asset %q, failure %q after %d requests", asset, failure, calls)
	}
}

func TestCatalogueFailureStages(t *testing.T) {
	for _, test := range []struct {
		storefront string
		status     int
		body       string
		want       string
	}{
		{"143472", 200, `{"data":[{"id":"143472"}]}`, "storefront"},
		{"143472", 403, "", "catalogueAuthorization"},
		{"za", 401, "", "catalogueAuthorization"},
		{"za", 404, "", "catalogueTrack"},
		{"za", 200, `{"data":[{"id":"1234"}]}`, "catalogueAsset"},
		{"za", 500, "", "catalogue"},
	} {
		account := drm.AccountInfo{StorefrontID: test.storefront, DevToken: "fixture-dev", MusicToken: "fixture-user"}
		client := &http.Client{Transport: roundTrip(func(*http.Request) (*http.Response, error) {
			return response(test.status, test.body), nil
		})}
		if asset, failure := catalogueALAC(context.Background(), client, account, "1234"); asset != "" || failure != test.want {
			t.Errorf("got asset %q, failure %q, wanted failure %q", asset, failure, test.want)
		}
	}
}

func response(status int, body string) *http.Response {
	return &http.Response{StatusCode: status, Body: io.NopCloser(strings.NewReader(body))}
}
