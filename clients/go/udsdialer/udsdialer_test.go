package udsdialer

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/h2c"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// fixturesPath is the shared, language-agnostic fixture file every
// implementation's test suite reads from rather than hand-typing its own
// table (architecture-review.md's test-duplication-drift Concern fix; see
// plan.md Pattern Decisions row 10 and validation.md row R2).
const fixturesPath = "../../../project_plans/unix-socket-auth/socket-path-fixtures.json"

type socketPathFixtures struct {
	DefaultPathCases []struct {
		Case     string            `json:"case"`
		Env      map[string]string `json:"env"`
		UID      int               `json:"uid"`
		Expected string            `json:"expected"`
	} `json:"default_path_cases"`
	ResolvePathCases []struct {
		Case     string            `json:"case"`
		Args     []string          `json:"args"`
		Env      map[string]string `json:"env"`
		UID      int               `json:"uid"`
		Expected string            `json:"expected"`
	} `json:"resolve_path_cases"`
}

func loadSocketPathFixtures(t *testing.T) socketPathFixtures {
	t.Helper()
	data, err := os.ReadFile(fixturesPath)
	if err != nil {
		t.Fatalf("reading shared fixture file %s: %v", fixturesPath, err)
	}
	var fixtures socketPathFixtures
	if err := json.Unmarshal(data, &fixtures); err != nil {
		t.Fatalf("parsing shared fixture file %s: %v", fixturesPath, err)
	}
	return fixtures
}

// setSocketPathEnv sets exactly the three env vars DefaultSocketPath/
// ResolveSocketPath read, defaulting any not present in env to "" —
// equivalent to unset for os.Getenv's purposes, and exercises the
// empty-string-treated-as-unset case identically to an actually-unset var.
// t.Setenv restores the pre-test value automatically, and its restriction
// against use with t.Parallel is fine here since these tests don't
// parallelize.
func setSocketPathEnv(t *testing.T, env map[string]string) {
	t.Helper()
	for _, name := range []string{"XDG_RUNTIME_DIR", "TMPDIR", "TYMUXD_SOCKET_PATH"} {
		t.Setenv(name, env[name])
	}
}

// TestDefaultSocketPath covers the five cases in the shared fixture file's
// default_path_cases (Task 7.1.1b): XDG set, XDG unset+TMPDIR set, both
// unset, XDG empty-string, and uid-scoping distinctness.
func TestDefaultSocketPath(t *testing.T) {
	fixtures := loadSocketPathFixtures(t)
	for _, c := range fixtures.DefaultPathCases {
		t.Run(c.Case, func(t *testing.T) {
			setSocketPathEnv(t, c.Env)
			got := DefaultSocketPath(c.UID)
			if got != c.Expected {
				t.Errorf("DefaultSocketPath(%d) = %q, want %q", c.UID, got, c.Expected)
			}
		})
	}
}

// TestResolveSocketPath covers the shared fixture file's resolve_path_cases
// that apply to this package: ResolveSocketPath has no CLI-flag layer of
// its own (only tymuxd and tymux-cli do), so cases whose fixture exercises
// "flag beats env"/"equals-joined flag form" (non-empty Args) are not
// applicable here and are skipped — only "env alone" and "neither present"
// apply, matching Task 7.1.1b's stated two cases.
func TestResolveSocketPath(t *testing.T) {
	fixtures := loadSocketPathFixtures(t)
	ran := 0
	for _, c := range fixtures.ResolvePathCases {
		if len(c.Args) > 0 {
			continue
		}
		ran++
		t.Run(c.Case, func(t *testing.T) {
			setSocketPathEnv(t, c.Env)
			got := ResolveSocketPath(c.UID)
			if got != c.Expected {
				t.Errorf("ResolveSocketPath(%d) = %q, want %q", c.UID, got, c.Expected)
			}
		})
	}
	if ran == 0 {
		t.Fatal("no applicable (flag-less) cases found in resolve_path_cases fixture")
	}
}

// fakeTymuxService implements just enough of tymuxv1connect's generated
// handler interface (ListSessions) to prove DialUnixHTTPClient's UDS+h2c
// wiring round-trips a real connect-go RPC end to end. It embeds
// UnimplementedTymuxServiceHandler for every other method.
//
// This does not spawn the real tymuxd binary: as of this writing,
// crates/tymuxd's dual-listener wiring (Phase 4 — main.rs binding
// auth::bind_uds_listener alongside its existing TCP listener) has not
// landed on this branch, so no tymuxd build can yet accept a UDS
// connection at all (concurrent work in another crate, out of this
// package's scope). Task 7.1.2b anticipates exactly this: "reusing this
// package's own startDaemon-equivalent" — a local, in-process fake server —
// as the simpler of its two suggested shapes. This test exercises the
// client-side dialing/transport logic this package owns; the real-daemon
// round trip is covered later by Task 7.3.1a's
// TestListSessionsSucceedsOverUDS once Phase 4 lands.
type fakeTymuxService struct {
	tymuxv1connect.UnimplementedTymuxServiceHandler
}

func (fakeTymuxService) ListSessions(context.Context, *connect.Request[tymuxv1.ListSessionsRequest]) (*connect.Response[tymuxv1.ListSessionsResponse], error) {
	return connect.NewResponse(&tymuxv1.ListSessionsResponse{}), nil
}

func TestDialUnixHTTPClientRoundTripsListSessions(t *testing.T) {
	socketPath := filepath.Join(t.TempDir(), "tymuxd.sock")
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatalf("net.Listen(unix, %s): %v", socketPath, err)
	}

	mux := http.NewServeMux()
	path, handler := tymuxv1connect.NewTymuxServiceHandler(fakeTymuxService{})
	mux.Handle(path, handler)
	server := &http.Server{Handler: h2c.NewHandler(mux, &http2.Server{})}
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { _ = server.Close() })

	client := tymuxv1connect.NewTymuxServiceClient(DialUnixHTTPClient(socketPath), "http://unix", connect.WithGRPC())

	resp, err := client.ListSessions(context.Background(), connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	if err != nil {
		t.Fatalf("ListSessions over UDS: %v", err)
	}
	if got := len(resp.Msg.GetSessions()); got != 0 {
		t.Fatalf("expected 0 sessions from the fake service, got %d", got)
	}
}
