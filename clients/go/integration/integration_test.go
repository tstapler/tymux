// Package integration proves tymux's cross-language claim (ADR-003) for Go
// the same way clients/ts/test/integration.test.ts does: it spawns a real
// tymuxd on an ephemeral loopback port and drives the generated Go client's
// CreateSession/ListSessions RPCs against it. Without this, CI only proved
// the generated client compiles — never that it can talk to a real daemon.
package integration

import (
	"bufio"
	"context"
	"crypto/tls"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// repoRoot resolves the repo root relative to this file
// (clients/go/integration -> clients/go -> clients -> repo root), mirroring
// clients/ts/test/daemon.ts's REPO_ROOT.
func repoRoot(t *testing.T) string {
	t.Helper()
	wd, err := os.Getwd()
	if err != nil {
		t.Fatalf("os.Getwd: %v", err)
	}
	return filepath.Join(wd, "..", "..", "..")
}

// resolveBinary mirrors daemon.ts's resolveBinary: prefer TYMUXD_BIN (set by
// the go-client CI job, matching ts-client's job), else fall back to a
// locally built debug/release binary.
func resolveBinary(t *testing.T) string {
	t.Helper()
	if bin := os.Getenv("TYMUXD_BIN"); bin != "" {
		return bin
	}
	root := repoRoot(t)
	for _, profile := range []string{"debug", "release"} {
		candidate := filepath.Join(root, "target", profile, "tymuxd")
		if _, err := os.Stat(candidate); err == nil {
			return candidate
		}
	}
	t.Fatal("tymuxd binary not found — build it first (cargo build --bin tymuxd) or set TYMUXD_BIN")
	return ""
}

// startDaemon spawns a real tymuxd on an ephemeral loopback port and waits
// for its "tymuxd listening" stdout line, same signal daemon.ts waits on.
func startDaemon(t *testing.T) string {
	t.Helper()

	port := 30000 + time.Now().UnixNano()%20000
	addr := fmt.Sprintf("127.0.0.1:%d", port)
	stateDir := t.TempDir()

	cmd := exec.Command(resolveBinary(t))
	cmd.Env = append(os.Environ(), "TYMUXD_ADDR="+addr, "XDG_STATE_HOME="+stateDir)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("StdoutPipe: %v", err)
	}
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		t.Fatalf("tymuxd start: %v", err)
	}
	t.Cleanup(func() {
		_ = cmd.Process.Signal(os.Interrupt)
		_ = cmd.Wait()
	})

	ready := make(chan struct{})
	go func() {
		scanner := bufio.NewScanner(stdout)
		for scanner.Scan() {
			if strings.Contains(scanner.Text(), "tymuxd listening") {
				close(ready)
				return
			}
		}
	}()

	select {
	case <-ready:
	case <-time.After(5 * time.Second):
		t.Fatal("tymuxd did not report listening within 5s")
	}

	return "http://" + addr
}

// newClient mirrors examples/list-sessions/main.go's newClient: tymuxd is a
// strict gRPC server (tonic) on plain h2c, no TLS.
func newClient(baseURL string) tymuxv1connect.TymuxServiceClient {
	httpClient := &http.Client{
		Transport: &http2.Transport{
			AllowHTTP: true,
			DialTLSContext: func(ctx context.Context, network, addr string, _ *tls.Config) (net.Conn, error) {
				return net.Dial(network, addr)
			},
		},
	}
	return tymuxv1connect.NewTymuxServiceClient(httpClient, baseURL, connect.WithGRPC())
}

// TestListSessionsReflectsCreateSession mirrors
// clients/ts/test/integration.test.ts's "listSessions reflects a session
// created via createSession" (Story 7.2 AC1): a real unary RPC round-trip
// through the generated client against a live daemon.
func TestListSessionsReflectsCreateSession(t *testing.T) {
	addr := startDaemon(t)
	client := newClient(addr)
	ctx := context.Background()

	created, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{
		Name:    "go-integration",
		Command: "",
	}))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}

	listed, err := client.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	if err != nil {
		t.Fatalf("ListSessions: %v", err)
	}

	var found *tymuxv1.Session
	for _, s := range listed.Msg.GetSessions() {
		if s.GetId() == created.Msg.GetId() {
			found = s
			break
		}
	}
	if found == nil {
		t.Fatalf("created session %q not found in ListSessions response", created.Msg.GetId())
	}
	if found.GetName() != "go-integration" {
		t.Errorf("got name %q, want %q", found.GetName(), "go-integration")
	}
}
