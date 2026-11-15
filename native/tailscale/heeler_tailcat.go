// Copyright (c) Heeler contributors
// SPDX-License-Identifier: BSD-3-Clause

// Heeler's tailcat client, joined to libtailscale's package main like
// heeler_tailscale.go.
//
// tailcat (github.com/tailscale/tailcat) reaches one `tailcat serve` peer
// through a WireGuard tunnel that is set up over a DERP relay, with no
// tailnet and no coordination server: everything a client needs is in the
// server's tailcat address. It is built from the Tailscale data plane tsnet
// already links (WireGuard, magicsock, DERP, netstack), so it lives in this
// archive rather than in one of its own: the app keeps one Go runtime and
// one copy of that data plane.
//
// A tailcat address carries the tunnel's pre-shared key, so it is a secret.
// Nothing here logs it or puts it in an error message. The client's own log
// lines, which heeler_tailcat_open sends to the caller's descriptor, name
// the server's public keys and not the pre-shared key.

package main

/*
#include <stddef.h>
#include <stdint.h>
*/
import "C"

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"syscall"
	"time"
	"unsafe"

	"github.com/tailscale/tailcat"
	"tailscale.com/net/netns"
)

// Result codes, mirrored by HEELER_TAILCAT_* in CTailscale.h.
const (
	tailcatOK         = 0
	tailcatErr        = -1
	tailcatErrTimeout = -2
	tailcatErrClosed  = -3
	tailcatErrAddress = -4
)

// tailcatClient is one open client: a tunnel to one tailcat server that
// every dial on the handle shares.
type tailcatClient struct {
	client  *tailcat.Client
	address string
	// ctx ends when the handle is closed, so a close from another thread
	// interrupts the handle's dials and pings.
	ctx    context.Context
	cancel context.CancelFunc
	log    *tailcatLog
}

// tailcatClients holds the open clients. A handle is never reused.
var tailcatClients struct {
	mu   sync.Mutex
	last C.int
	m    map[C.int]*tailcatClient
}

func tailcatLookup(handle C.int) *tailcatClient {
	tailcatClients.mu.Lock()
	defer tailcatClients.mu.Unlock()
	return tailcatClients.m[handle]
}

// tailcatLog writes a client's log lines to a descriptor the caller owns.
// Writes go through write(2) on the number itself: an os.File would close
// the caller's descriptor when it is collected.
type tailcatLog struct {
	mu sync.Mutex
	fd int // -1 discards
}

func (l *tailcatLog) logf(format string, args ...any) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.fd < 0 {
		return
	}
	line := []byte(fmt.Sprintf(format, args...) + "\n")
	for len(line) > 0 {
		n, err := syscall.Write(l.fd, line)
		if err != nil || n <= 0 {
			return
		}
		line = line[n:]
	}
}

// stop ends the writes. tailcat's goroutines can outlive Close and still
// log; none of that reaches the descriptor once stop has returned.
func (l *tailcatLog) stop() {
	l.mu.Lock()
	l.fd = -1
	l.mu.Unlock()
}

var errTailcatAddress = errors.New("not a tailcat address")

// tailcatParse decodes a tailcat address and refuses one a client cannot
// use. Its errors never quote the address.
func tailcatParse(address string) (tailcat.ConnInfo, error) {
	info, err := tailcat.ParseAddr(tailcat.Addr(address))
	if err != nil {
		return info, errTailcatAddress
	}
	if info.ServerDiscoPublic.IsZero() {
		return info, errors.New("this tailcat address has no path-discovery key, so an old tailcat server made it; update tailcat on the server and use its new address")
	}
	return info, nil
}

// tailcatRestoreNetns turns netns back on. tailcat turns it off for the
// whole process when it builds a client's engine (netns.SetEnabled(false) in
// its createEngine), as tailscaled does without a TUN. tsnet leaves it on,
// so from the first tailcat dial the tsnet nodes in this process would stop
// binding new sockets to the default route's interface. Restoring it after
// every call that can build an engine keeps them as they are; a tailcat
// client then binds the sockets it opens later the same way. While such a
// call runs, sockets either stack opens are not bound.
func tailcatRestoreNetns() {
	netns.SetEnabled(true)
}

// tailcatMessage writes a NUL-terminated message (truncated to fit) into a
// C error buffer, which may be NULL.
func tailcatMessage(out *C.char, length C.size_t, message string) {
	if out == nil || length == 0 {
		return
	}
	buffer := unsafe.Slice((*byte)(unsafe.Pointer(out)), int(length))
	n := copy(buffer[:len(buffer)-1], message)
	buffer[n] = 0
}

// tailcatFailure reports err: its code, and its text with the client's
// address removed.
func tailcatFailure(err error, address string, out *C.char, length C.size_t) C.int {
	message := err.Error()
	if address != "" {
		message = strings.ReplaceAll(message, address, "<address>")
	}
	tailcatMessage(out, length, message)
	switch {
	case errors.Is(err, context.DeadlineExceeded):
		return tailcatErrTimeout
	case errors.Is(err, context.Canceled):
		return tailcatErrClosed
	default:
		return tailcatErr
	}
}

// tailcatJSON writes value as NUL-terminated JSON and returns its length
// without the terminator. A result >= length means nothing was written.
func tailcatJSON(value any, out *C.char, length C.size_t) C.int {
	encoded, err := json.Marshal(value)
	if err != nil {
		return tailcatErr
	}
	if out != nil && C.size_t(len(encoded)) < length {
		buffer := unsafe.Slice((*byte)(unsafe.Pointer(out)), int(length))
		copy(buffer, encoded)
		buffer[len(encoded)] = 0
	}
	return C.int(len(encoded))
}

func tailcatTimeout(milliseconds C.uint32_t) time.Duration {
	if milliseconds == 0 {
		return time.Millisecond
	}
	return time.Duration(milliseconds) * time.Millisecond
}

// heeler_tailcat_address_info describes a tailcat address without touching
// the network: whether it carries a pre-shared key, and where its relay
// comes from (a region of the relay map the client fetches, or relays the
// address names itself). The JSON never holds a key.
//
//export heeler_tailcat_address_info
func heeler_tailcat_address_info(address *C.char, out *C.char, length C.size_t) C.int {
	info, err := tailcatParse(C.GoString(address))
	if err != nil {
		return tailcatErrAddress
	}
	description := struct {
		PresharedKey bool     `json:"preshared_key"`
		RegionID     *int     `json:"region_id"`
		Relays       []string `json:"relays"`
	}{
		PresharedKey: !info.PresharedKey.IsZero(),
		Relays:       []string{},
	}
	for _, region := range info.Region {
		for _, node := range region.Nodes {
			description.Relays = append(description.Relays, node.HostName)
		}
	}
	if len(info.Region) == 0 && info.RegionID > 0 {
		id := int(info.RegionID)
		description.RegionID = &id
	}
	return tailcatJSON(description, out, length)
}

// heeler_tailcat_open creates a client for the server a tailcat address
// names and returns its handle. Nothing reaches the network until the first
// dial or ping. Log lines go to logFD until heeler_tailcat_close returns
// (-1 discards them); the caller owns the descriptor.
//
//export heeler_tailcat_open
func heeler_tailcat_open(address *C.char, logFD C.int, errOut *C.char, errLen C.size_t) C.int {
	server := C.GoString(address)
	if _, err := tailcatParse(server); err != nil {
		tailcatMessage(errOut, errLen, err.Error())
		return tailcatErrAddress
	}
	log := &tailcatLog{fd: int(logFD)}
	client := tailcat.NewClient(tailcat.Addr(server))
	client.Logf = log.logf
	ctx, cancel := context.WithCancel(context.Background())
	opened := &tailcatClient{client: client, address: server, ctx: ctx, cancel: cancel, log: log}

	tailcatClients.mu.Lock()
	defer tailcatClients.mu.Unlock()
	if tailcatClients.m == nil {
		tailcatClients.m = make(map[C.int]*tailcatClient)
	}
	tailcatClients.last++
	tailcatClients.m[tailcatClients.last] = opened
	tailcatMessage(errOut, errLen, "")
	return tailcatClients.last
}

// heeler_tailcat_dial opens a TCP stream to port on the client's server and
// returns one end of a socketpair, as tailscale_dial does: libtailscale
// pumps the other end and ends the stream when the descriptor is closed.
// The first dial brings the tunnel up, within the same timeout.
//
//export heeler_tailcat_dial
func heeler_tailcat_dial(handle C.int, port C.uint16_t, timeoutMillis C.uint32_t, errOut *C.char, errLen C.size_t) C.int {
	opened := tailcatLookup(handle)
	if opened == nil {
		tailcatMessage(errOut, errLen, "the tailcat client is closed")
		return tailcatErrClosed
	}
	if port == 0 {
		tailcatMessage(errOut, errLen, "port 0 is not a tailcat service port")
		return tailcatErr
	}
	ctx, cancel := context.WithTimeout(opened.ctx, tailcatTimeout(timeoutMillis))
	defer cancel()
	remote, err := opened.client.DialTCPPort(ctx, uint16(port))
	tailcatRestoreNetns()
	if err != nil {
		return tailcatFailure(err, opened.address, errOut, errLen)
	}
	var descriptor C.int
	// newConn reads only the tsnet server it files the stream under,
	// which a tailcat stream does not have.
	if err := newConn(&server{}, remote, &descriptor); err != nil {
		remote.Close()
		return tailcatFailure(err, opened.address, errOut, errLen)
	}
	tailcatMessage(errOut, errLen, "")
	return descriptor
}

// heeler_tailcat_ping measures the path to the server and reports whether
// it is direct or through a relay. Like a dial, the first ping brings the
// tunnel up. Each ping also prompts both ends to look for a direct path.
//
//export heeler_tailcat_ping
func heeler_tailcat_ping(handle C.int, timeoutMillis C.uint32_t, out *C.char, length C.size_t, errOut *C.char, errLen C.size_t) C.int {
	opened := tailcatLookup(handle)
	if opened == nil {
		tailcatMessage(errOut, errLen, "the tailcat client is closed")
		return tailcatErrClosed
	}
	ctx, cancel := context.WithTimeout(opened.ctx, tailcatTimeout(timeoutMillis))
	defer cancel()
	result, err := opened.client.DiscoPing(ctx)
	tailcatRestoreNetns()
	if err != nil {
		return tailcatFailure(err, opened.address, errOut, errLen)
	}
	path := struct {
		LatencyMillis float64 `json:"latency_ms"`
		Direct        bool    `json:"direct"`
		Endpoint      *string `json:"endpoint"`
		Relay         *string `json:"relay"`
	}{
		LatencyMillis: result.LatencySeconds * 1000,
		Direct:        result.Endpoint != "",
	}
	if path.Direct {
		path.Endpoint = &result.Endpoint
	} else if result.DERPRegionCode != "" {
		path.Relay = &result.DERPRegionCode
	}
	tailcatMessage(errOut, errLen, "")
	return tailcatJSON(path, out, length)
}

// heeler_tailcat_close closes a client: its tunnel, its relay connection,
// and the streams dialled through it, which end at their descriptors.
// Dials and pings waiting on the handle return HEELER_TAILCAT_ERR_CLOSED.
//
//export heeler_tailcat_close
func heeler_tailcat_close(handle C.int) C.int {
	tailcatClients.mu.Lock()
	opened := tailcatClients.m[handle]
	delete(tailcatClients.m, handle)
	tailcatClients.mu.Unlock()
	if opened == nil {
		return tailcatErrClosed
	}
	opened.cancel()
	opened.client.Close()
	opened.log.stop()
	return tailcatOK
}
