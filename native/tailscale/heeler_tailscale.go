// Copyright (c) Heeler contributors
// SPDX-License-Identifier: BSD-3-Clause

// Heeler's addition to libtailscale's package main, copied into the pinned
// libtailscale source tree by Scripts/build-native.sh.
//
// tsnet uploads its logs to log.tailscale.com. Setting TS_NO_LOGS_NO_SUPPORT
// from the host app with setenv(3) has no effect: the Go runtime copies the
// environment once at startup and os.Getenv never sees later C changes. And
// tsnet does not consult that knob before starting its logtail uploader
// anyway. These exports switch both off from inside the Go runtime.
//
// libtailscale also has no logout; heeler_tailscale_logout reaches tsnet's
// LocalAPI for it.

package main

// #include <errno.h>
import "C"

import (
	"context"
	"errors"
	"io"
	"net/http"
	"sync"
	"time"

	"tailscale.com/envknob"
	"tailscale.com/logtail"
)

// heeler_tailscale_disable_log_upload opts the process out of log uploads:
// it sets TS_NO_LOGS_NO_SUPPORT through envknob (so tailscale.com code that
// asks envknob.NoLogsNoSupport sees it) and disables logtail, which stops
// every logtail.Logger, including tsnet's, from buffering or uploading.
// Call it before the first tailscale_new; it is idempotent.
//
//export heeler_tailscale_disable_log_upload
func heeler_tailscale_disable_log_upload() {
	envknob.SetNoLogsNoSupport()
	logtail.Disable()
}

// heeler_tailscale_logout signs the started server sd out: LocalAPI's
// logout tells the coordination server and drops the node key from the
// server's state, waiting at most timeoutMillis. A node that was never
// logged in returns 0 at once. On failure it returns -1 and records the
// reason for tailscale_errmsg; an unknown sd is EBADF.
//
//export heeler_tailscale_logout
func heeler_tailscale_logout(sd C.int, timeoutMillis C.int) C.int {
	s := getServer(sd)
	if s == nil {
		return C.EBADF
	}
	lc, err := s.s.LocalClient()
	if err != nil {
		return s.recErr(err)
	}
	timeout := time.Duration(timeoutMillis) * time.Millisecond
	if timeout <= 0 {
		timeout = time.Millisecond
	}
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	return s.recErr(lc.Logout(ctx))
}

// heeler_tailscale_log_upload_state reports what the Go side sees: bit 0 is
// set when envknob.NoLogsNoSupport() is true, bit 1 when logtail drops log
// lines instead of buffering them for upload. Both bits set (3) means log
// upload is off.
//
//export heeler_tailscale_log_upload_state
func heeler_tailscale_log_upload_state() C.int {
	var state C.int
	if envknob.NoLogsNoSupport() {
		state |= 1
	}
	if !logtailBuffersLines() {
		state |= 2
	}
	return state
}

// logtailBuffersLines writes one line through a throwaway logtail.Logger
// whose buffer only counts writes and whose HTTP client refuses every
// request, so the probe itself can never reach the network.
func logtailBuffersLines() bool {
	buffer := &countingBuffer{}
	logger := logtail.NewLogger(logtail.Config{
		Collection:   "heeler-probe.invalid",
		BaseURL:      "http://127.0.0.1:9",
		HTTPC:        &http.Client{Transport: refuseTransport{}},
		Stderr:       io.Discard,
		Buffer:       buffer,
		FlushDelayFn: func() time.Duration { return time.Hour },
	}, func(string, ...any) {})
	logger.Logf("heeler log upload probe")
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	logger.Shutdown(ctx)
	return buffer.count() > 0
}

type countingBuffer struct {
	mu     sync.Mutex
	writes int
}

func (b *countingBuffer) TryReadLine() ([]byte, error) { return nil, nil }

func (b *countingBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	b.writes++
	b.mu.Unlock()
	return len(p), nil
}

func (b *countingBuffer) count() int {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.writes
}

type refuseTransport struct{}

func (refuseTransport) RoundTrip(*http.Request) (*http.Response, error) {
	return nil, errors.New("heeler log upload probe: network disabled")
}
