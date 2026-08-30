// Package udsdialer provides Go clients with the same Unix-domain-socket
// default-path algorithm tymuxd uses, plus a constructor for an
// *http.Client that dials that socket over h2c. Mirrors authinterceptor's
// shape: a single, reusable, dependency-light package shared by the
// examples and integration tests instead of each hand-rolling its own copy.
package udsdialer

import (
	"context"
	"crypto/tls"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"

	"golang.org/x/net/http2"
)

// DefaultSocketPath mirrors tymuxd's auth::default_uds_socket_path and
// tymux-cli's copy of the same algorithm byte-for-byte — see
// project_plans/unix-socket-auth/implementation/plan.md Pattern Decisions
// row 10. Any change must be mirrored in all four implementations
// (tymuxd, tymux-cli, clients/go, clients/ts).
func DefaultSocketPath(uid int) string {
	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, "tymuxd", "tymuxd.sock")
	}
	base := os.Getenv("TMPDIR")
	if base == "" {
		base = "/tmp"
	}
	return filepath.Join(base, "tymuxd-"+strconv.Itoa(uid), "tymuxd.sock")
}

// ResolveSocketPath applies the TYMUXD_SOCKET_PATH override, matching
// resolve_uds_socket_path's flag-beats-env precedence — Go clients in this
// package have no CLI-flag layer of their own, so only the env var is
// checked here; a caller with its own flag parsing (e.g. a future
// tymux-go-cli) should check its flag first and fall back to this function.
func ResolveSocketPath(uid int) string {
	if p := os.Getenv("TYMUXD_SOCKET_PATH"); p != "" {
		return p
	}
	return DefaultSocketPath(uid)
}

// DialUnixHTTPClient returns an *http.Client wired to dial socketPath over
// h2c (plaintext HTTP/2 — tymuxd is a strict gRPC/tonic server with no TLS
// on either its TCP or UDS listener, per the loopback-trust security model
// ADR) regardless of the network/addr a caller's generated connect-go
// client passes at request time. Mirrors examples/list-sessions/main.go's
// existing newClient shape, replacing net.Dial(network, addr) with a fixed
// "unix" dial to socketPath — the seam research/stack.md §4 identified.
func DialUnixHTTPClient(socketPath string) *http.Client {
	return &http.Client{
		Transport: &http2.Transport{
			AllowHTTP: true,
			DialTLSContext: func(ctx context.Context, _, _ string, _ *tls.Config) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", socketPath)
			},
		},
	}
}
