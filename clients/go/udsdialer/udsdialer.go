// Package udsdialer provides Go clients with the same Unix-domain-socket
// default-path algorithm tymuxd uses, plus a constructor for an
// *http.Client that dials that socket over h2c. Mirrors authinterceptor's
// shape: a single, reusable, dependency-light package shared by the
// examples and integration tests instead of each hand-rolling its own copy.
package udsdialer

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"syscall"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	"github.com/tstapler/tymux/clients/go/authinterceptor"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
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

// newTCPClient builds a TymuxServiceClient against the deprecated TCP
// loopback address. tymuxd listens on loopback-only plain HTTP/2 (h2c, no
// TLS — see the loopback-trust security model ADR) and is a strict gRPC
// server (tonic), so the client needs an h2c-capable transport and
// connect.WithGRPC(). token is attached via authinterceptor.Interceptor on
// every outgoing call; an empty token is a no-op.
func newTCPClient(baseURL, token string) tymuxv1connect.TymuxServiceClient {
	httpClient := &http.Client{
		Transport: &http2.Transport{
			AllowHTTP: true,
			DialTLSContext: func(ctx context.Context, network, addr string, _ *tls.Config) (net.Conn, error) {
				return net.Dial(network, addr)
			},
		},
	}
	return tymuxv1connect.NewTymuxServiceClient(httpClient, baseURL, connect.WithGRPC(),
		connect.WithInterceptors(authinterceptor.Interceptor{Token: token}))
}

// DialWithFallback tries ResolveSocketPath(os.Getuid())'s Unix socket
// first, falling back to the deprecated TCP loopback address on failure —
// mirrors tymux-cli's dial_channel shape (Epic 7.2, Task 7.2.1b). Extracted
// here (rather than duplicated per-example, as it originally shipped) so
// the EACCES-vs-fallback security decision below has exactly one
// implementation for every Go example/CLI to share.
//
// The probe dial's error is classified per pre-mortem.md P1 #1:
// syscall.EACCES means a daemon IS listening at the resolved socket path
// and the kernel denied this connect() itself, so it is a hard error that
// never falls back to the unauthenticated TCP path; anything else
// (syscall.ENOENT — no socket file, syscall.ECONNREFUSED — file present but
// nothing listening, or any other error) means no daemon is there, so
// falling back is legitimate and logged to stderr.
func DialWithFallback(token string) (tymuxv1connect.TymuxServiceClient, error) {
	socketPath := ResolveSocketPath(os.Getuid())
	conn, err := net.Dial("unix", socketPath)
	if err != nil {
		if errors.Is(err, syscall.EACCES) {
			return nil, fmt.Errorf(
				"tymuxd rejected connection to %s: not authorized to access this daemon's "+
					"socket (ask the daemon's owner to add you to its configured "+
					"--socket-group, or run this client as the daemon's own OS user): %w",
				socketPath, err)
		}
		fmt.Fprintf(os.Stderr,
			"no reachable Unix socket at %s -- falling back to TCP loopback "+
				"(deprecated; make sure tymuxd is running)\n", socketPath)
		return newTCPClient("http://127.0.0.1:7419", token), nil
	}
	_ = conn.Close() // probe only; DialUnixHTTPClient/http2.Transport does its own dialing per-request
	return tymuxv1connect.NewTymuxServiceClient(DialUnixHTTPClient(socketPath), "http://unix",
		connect.WithGRPC(), connect.WithInterceptors(authinterceptor.Interceptor{Token: token})), nil
}
