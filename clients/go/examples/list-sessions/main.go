// Command list-sessions proves tymux's cross-language claim (ADR-003) for
// Go: a client generated straight from proto/tymux/v1/tymux.proto via buf
// + protoc-gen-go + protoc-gen-connect-go, driving a real tymuxd daemon's
// ListSessions RPC — no Rust code involved. Mirrors
// clients/ts/examples/list-sessions.ts's shape and output.
package main

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"syscall"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	"github.com/tstapler/tymux/clients/go/authinterceptor"
	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// tymuxd listens on loopback-only plain HTTP/2 (h2c, no TLS — see the
// loopback-trust security model ADR), and is a strict gRPC server (tonic),
// so the client needs an h2c-capable transport and connect.WithGRPC(). token
// is attached via authinterceptor.Interceptor on every outgoing call; an
// empty token is a no-op, so this example still works unmodified against a
// loopback, non-token-gated daemon.
func newClient(baseURL, token string) tymuxv1connect.TymuxServiceClient {
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

// dialClient tries udsdialer.ResolveSocketPath(os.Getuid())'s Unix socket
// first, falling back to the deprecated TCP loopback address on failure —
// mirrors tymux-cli's dial_channel shape (Epic 7.2, Task 7.2.1b). The probe
// dial's error is classified exactly like dial_channel's fix for
// pre-mortem.md P1 #1: syscall.EACCES means a daemon IS listening at
// socketPath and the kernel denied this connect() itself, so it is a hard
// error that never falls back to the unauthenticated TCP path; anything
// else (syscall.ENOENT — no socket file, syscall.ECONNREFUSED — file
// present but nothing listening, or any other error) means no daemon is
// there, so falling back is legitimate and logged to stderr.
func dialClient(token string) (tymuxv1connect.TymuxServiceClient, error) {
	socketPath := udsdialer.ResolveSocketPath(os.Getuid())
	conn, err := net.Dial("unix", socketPath)
	if err != nil {
		if errors.Is(err, syscall.EACCES) {
			return nil, fmt.Errorf(
				"tymuxd rejected this connection: not authorized to access this daemon's " +
					"socket (ask the daemon's owner to add you to its configured " +
					"--socket-group, or run this client as the daemon's own OS user)")
		}
		fmt.Fprintf(os.Stderr,
			"no reachable Unix socket at %s -- falling back to TCP loopback "+
				"(deprecated; make sure tymuxd is running)\n", socketPath)
		return newClient("http://127.0.0.1:7419", token), nil
	}
	conn.Close() // probe only; DialUnixHTTPClient/http2.Transport does its own dialing per-request
	return tymuxv1connect.NewTymuxServiceClient(udsdialer.DialUnixHTTPClient(socketPath), "http://unix",
		connect.WithGRPC(), connect.WithInterceptors(authinterceptor.Interceptor{Token: token})), nil
}

// flattenPaneIDs walks a window's layout tree in tree order, returning
// every leaf pane_id — same traversal as list-sessions.ts's flattenPaneIds.
func flattenPaneIDs(layout *tymuxv1.Layout) []string {
	if layout == nil {
		return nil
	}
	switch node := layout.GetNode().(type) {
	case *tymuxv1.Layout_Pane:
		return []string{node.Pane.GetId()}
	case *tymuxv1.Layout_Split:
		var ids []string
		for _, child := range node.Split.GetChildren() {
			ids = append(ids, flattenPaneIDs(child.GetLayout())...)
		}
		return ids
	default:
		return nil
	}
}

func main() {
	client, err := dialClient(os.Getenv("TYMUXD_TOKEN"))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	resp, err := client.ListSessions(context.Background(), connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	if err != nil {
		fmt.Fprintln(os.Stderr, "ListSessions failed:", err)
		os.Exit(1)
	}

	for _, session := range resp.Msg.GetSessions() {
		fmt.Printf("%s\t%s\t%s\n", session.GetId(), session.GetName(), session.GetLiveness())
		for _, window := range session.GetWindows() {
			for _, paneID := range flattenPaneIDs(window.GetLayout()) {
				fmt.Printf("  window %s (%s)\tpane %s\n", window.GetName(), window.GetId(), paneID)
			}
		}
	}
}
