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
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	"github.com/tstapler/tymux/clients/go/authinterceptor"
	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// flattenPaneIDs walks a window's layout tree in tree order, returning
// every leaf pane_id. Duplicated from examples/list-sessions/main.go
// (package main, not importable from here) rather than shared — same
// precedent as this file's own newClient duplicating
// examples/list-sessions/main.go's.
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

// firstPaneID extracts the id of the first leaf pane in a freshly created
// session's sole window.
func firstPaneID(t *testing.T, session *tymuxv1.Session) string {
	t.Helper()
	if len(session.GetWindows()) == 0 {
		t.Fatal("created session has no windows")
	}
	ids := flattenPaneIDs(session.GetWindows()[0].GetLayout())
	if len(ids) == 0 {
		t.Fatal("created session's sole window has no panes")
	}
	return ids[0]
}

// seqPtr returns a pointer to v — AttachRequest.ResumeFromSeq is a
// proto3-optional scalar, so a resume-capable client threads a pointer, not
// a bare uint64, to distinguish "no resume state" (nil) from a real,
// possibly-zero seq.
func seqPtr(v uint64) *uint64 { return &v }

// runAttachUntilMarker attaches to paneID (optionally with resumeFromSeq),
// sends cmd as pty input (skipped when cmd is empty), and returns the
// concatenated OutputChunk bytes received up to and including marker's own
// text — truncated right there even if the chunk that completed the match
// carried further bytes past it (e.g. the shell's own asynchronously-timed
// next-prompt redraw, which can land in the same read/chunk burst or not,
// unpredictably). Two independently captured streams need to agree on
// exactly this cutoff to ever compare byte-identical (Task 5.2.1b) —
// without truncation, whichever stream's prompt redraw happened to win the
// race into the same chunk as the marker would carry extra bytes the other
// one doesn't.
func runAttachUntilMarker(t *testing.T, client tymuxv1connect.TymuxServiceClient, paneID string, resumeFromSeq *uint64, cmd, marker string, timeout time.Duration) []byte {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	stream := client.Attach(ctx)
	if err := stream.Send(&tymuxv1.AttachRequest{
		Payload:       &tymuxv1.AttachRequest_PaneId{PaneId: paneID},
		ResumeFromSeq: resumeFromSeq,
	}); err != nil {
		t.Fatalf("send pane_id: %v", err)
	}
	if cmd != "" {
		if err := stream.Send(&tymuxv1.AttachRequest{Payload: &tymuxv1.AttachRequest_Input{Input: []byte(cmd)}}); err != nil {
			t.Fatalf("send input: %v", err)
		}
	}

	var buf []byte
	for {
		if idx := strings.Index(string(buf), marker); idx != -1 {
			return buf[:idx+len(marker)]
		}
		event, err := stream.Receive()
		if err != nil {
			t.Fatalf("receive (waiting for %q): %v, got so far: %q", marker, err, buf)
		}
		if chunk, ok := event.GetPayload().(*tymuxv1.AttachEvent_OutputChunk); ok {
			buf = append(buf, chunk.OutputChunk.GetData()...)
		}
	}
}

// TestAttachResumesByteIdenticalAfterDisconnectAndReattachWithRecordedSeq
// mirrors Story 5.1.1's TS resume test (Task 5.1.1b) for the Go client:
// disconnect mid-stream, reattach with the last-seen seq, and assert the
// reconstructed output is byte-identical to what a client that never
// disconnected would have seen. Unlike the daemon-side unit test of the
// same scenario (crates/tymuxd/src/main.rs's
// attach_should_replay_missed_chunks_byte_identical_..._when_resume_from_seq_in_window,
// which uses the internal Pane API as ground truth), this is a black-box
// RPC-only client test with no access to daemon internals — so the ground
// truth is a second, independent pane running the identical deterministic
// command without ever disconnecting.
func TestAttachResumesByteIdenticalAfterDisconnectAndReattachWithRecordedSeq(t *testing.T) {
	addr := startDaemon(t)
	client := newClient(addr, "")
	ctx := context.Background()

	// Deterministic, non-timestamped output so two independent shell
	// processes produce byte-identical bytes: a short paced loop (sleeps
	// give the disconnect below a real mid-stream window to land in)
	// followed by a completion marker.
	// The completion marker is split across two adjacent quoted strings
	// ('RESUME-TEST-D' + 'ONE', concatenated by the shell) so the literal
	// keystroke bytes echoed back by the pty before execution never
	// contain the contiguous marker text — otherwise a naive
	// strings.Contains check below would match on the command's own echo
	// instead of its actual stdout.
	const cmd = "i=0; while [ $i -lt 40 ]; do printf 'M%03dE\\n' \"$i\"; i=$((i+1)); sleep 0.03; done; printf 'RESUME-TEST-D''ONE\\n'\n"
	const doneMarker = "RESUME-TEST-DONE"
	const partialMarker = "M015E"

	refSession, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{Name: "go-integration-resume-ref"}))
	if err != nil {
		t.Fatalf("CreateSession (reference): %v", err)
	}
	refPaneID := firstPaneID(t, refSession.Msg)
	// Passing resume_from_seq=Some(0) (rather than omitting it) is what
	// opts a client into the OutputChunk sibling field at all (Task
	// 2.2.1c: emit_output_chunk is only true when resume_from_seq is
	// present, on either the InWindow-empty or GapExceeded-fallback
	// branch) — an attach with no resume_from_seq at all gets only the
	// legacy, seq-less Output field forever, which this seq-tracking test
	// can't use.
	reference := runAttachUntilMarker(t, client, refPaneID, seqPtr(0), cmd, doneMarker, 15*time.Second)

	session, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{Name: "go-integration-resume"}))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	paneID := firstPaneID(t, session.Msg)

	firstCtx, cancelFirst := context.WithTimeout(context.Background(), 15*time.Second)
	stream1 := client.Attach(firstCtx)
	if err := stream1.Send(&tymuxv1.AttachRequest{
		Payload:       &tymuxv1.AttachRequest_PaneId{PaneId: paneID},
		ResumeFromSeq: seqPtr(0),
	}); err != nil {
		t.Fatalf("send pane_id: %v", err)
	}
	if err := stream1.Send(&tymuxv1.AttachRequest{Payload: &tymuxv1.AttachRequest_Input{Input: []byte(cmd)}}); err != nil {
		t.Fatalf("send input: %v", err)
	}

	var firstLeg []byte
	var lastSeq uint64
	for !strings.Contains(string(firstLeg), partialMarker) {
		event, err := stream1.Receive()
		if err != nil {
			t.Fatalf("receive (first leg): %v, got so far: %q", err, firstLeg)
		}
		if chunk, ok := event.GetPayload().(*tymuxv1.AttachEvent_OutputChunk); ok {
			firstLeg = append(firstLeg, chunk.OutputChunk.GetData()...)
			lastSeq = chunk.OutputChunk.GetSeq()
		}
	}
	cancelFirst() // simulate an abrupt disconnect mid-stream

	secondCtx, cancelSecond := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancelSecond()
	stream2 := client.Attach(secondCtx)
	if err := stream2.Send(&tymuxv1.AttachRequest{
		Payload:       &tymuxv1.AttachRequest_PaneId{PaneId: paneID},
		ResumeFromSeq: &lastSeq,
	}); err != nil {
		t.Fatalf("send pane_id (resume): %v", err)
	}

	// Truncated at doneMarker's own text for the same reason
	// runAttachUntilMarker truncates its reference capture — see its
	// doc comment.
	var secondLeg []byte
	seenSeq := lastSeq
	for {
		if idx := strings.Index(string(secondLeg), doneMarker); idx != -1 {
			secondLeg = secondLeg[:idx+len(doneMarker)]
			break
		}
		event, err := stream2.Receive()
		if err != nil {
			t.Fatalf("receive (resumed): %v, got so far: %q", err, secondLeg)
		}
		chunk, ok := event.GetPayload().(*tymuxv1.AttachEvent_OutputChunk)
		if !ok {
			t.Fatalf("expected only OutputChunk events on the resume path, got %T", event.GetPayload())
		}
		if chunk.OutputChunk.GetSeq() <= seenSeq {
			t.Fatalf("resumed seq must strictly increase past the recorded resume point: last=%d, got=%d", seenSeq, chunk.OutputChunk.GetSeq())
		}
		seenSeq = chunk.OutputChunk.GetSeq()
		secondLeg = append(secondLeg, chunk.OutputChunk.GetData()...)
	}

	reconstructed := append(append([]byte{}, firstLeg...), secondLeg...)
	if string(reconstructed) != string(reference) {
		t.Fatalf("resumed output does not match the never-disconnected reference:\n--- reconstructed ---\n%q\n--- reference ---\n%q", reconstructed, reference)
	}
}

// TestAttachReceivesGapExceededThenSnapshotWhenResumeFromSeqIsStaleAndEvicted
// mirrors Task 5.2.1c / crates/tymuxd/src/main.rs's
// attach_should_emit_gap_exceeded_then_snapshot_when_resume_from_seq_is_stale_and_evicted:
// a resume_from_seq older than anything the replay buffer still retains
// must produce GapExceeded{oldest_available_seq} as the very first event,
// followed immediately by a Snapshot.
func TestAttachReceivesGapExceededThenSnapshotWhenResumeFromSeqIsStaleAndEvicted(t *testing.T) {
	addr := startDaemon(t)
	client := newClient(addr, "")
	ctx := context.Background()

	session, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{Name: "go-integration-gap"}))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	paneID := firstPaneID(t, session.Msg)

	// Flood well past DEFAULT_REPLAY_BUFFER_BYTES (256 KiB): 40,000 lines
	// of "L0000000E\n" (10 bytes each) is ~390 KiB, evicting the pane's
	// earliest retained chunk(s) from its ReplayBuffer — same magnitude as
	// the daemon-side unit test this mirrors, driven purely over RPC here
	// since a black-box client test has no access to the internal Pane
	// API that test uses to write input directly.
	// Split-quote trick again (see the byte-identical test's comment):
	// keeps the literal "FLOOD-DONE" text out of the echoed keystrokes.
	const floodCmd = "i=0; while [ $i -lt 40000 ]; do printf 'L%07dE\\n' \"$i\"; i=$((i+1)); done; printf 'FLOOD-D''ONE\\n'\n"
	// resume_from_seq=Some(0) opts this attach into the OutputChunk field
	// (see the byte-identical test's comment on the same pattern) so the
	// flood's own chunks are observable at all; a pane's first-ever chunk
	// is always seq == 1 (Epic 2.1's documented invariant), so
	// resume_from_seq=1 below is guaranteed stale once the flood has
	// evicted the earliest retained chunks.
	_ = runAttachUntilMarker(t, client, paneID, seqPtr(0), floodCmd, "FLOOD-DONE", 30*time.Second)

	staleResumeFromSeq := uint64(1)
	resumeCtx, cancelResume := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancelResume()
	stream := client.Attach(resumeCtx)
	if err := stream.Send(&tymuxv1.AttachRequest{
		Payload:       &tymuxv1.AttachRequest_PaneId{PaneId: paneID},
		ResumeFromSeq: &staleResumeFromSeq,
	}); err != nil {
		t.Fatalf("send pane_id (stale resume): %v", err)
	}

	first, err := stream.Receive()
	if err != nil {
		t.Fatalf("receive (expected GapExceeded): %v", err)
	}
	gap, ok := first.GetPayload().(*tymuxv1.AttachEvent_GapExceeded)
	if !ok {
		t.Fatalf("expected first event to be GapExceeded, got %T", first.GetPayload())
	}
	if gap.GapExceeded.GetOldestAvailableSeq() == 0 {
		t.Errorf("expected a nonzero oldest_available_seq, got 0")
	}

	second, err := stream.Receive()
	if err != nil {
		t.Fatalf("receive (expected Snapshot): %v", err)
	}
	if _, ok := second.GetPayload().(*tymuxv1.AttachEvent_Snapshot); !ok {
		t.Fatalf("expected second event to be Snapshot immediately after GapExceeded, got %T", second.GetPayload())
	}
}

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
	return startDaemonOn(t, fmt.Sprintf("127.0.0.1:%d", ephemeralPort()), "")
}

// startDaemonWithToken spawns a real tymuxd bound non-loopback (0.0.0.0) on
// an ephemeral port, with TYMUXD_TOKEN=token in the spawned process's env —
// mirroring crates/tymuxd's Story 1.2.2b harness shape and daemon.ts's
// identically-shaped StartDaemonOptions{token}. A loopback bind never
// enforces auth (crates/tymuxd/src/main.rs's own startup gate), so proving
// reject/accept behavior against a token-gated daemon needs a real
// non-loopback socket. Existing tests keep using the loopback/no-token
// startDaemon above unchanged.
func startDaemonWithToken(t *testing.T, token string) string {
	t.Helper()
	return startDaemonOn(t, fmt.Sprintf("0.0.0.0:%d", ephemeralPort()), token)
}

// ephemeralPort picks a pseudo-random port in a fixed high range, shared by
// startDaemon and startDaemonWithToken so the two stay in sync by
// construction. A bind-then-close probe would avoid collision risk
// entirely, but this mirrors the pre-existing scheme startDaemon already
// used before this feature — not changing the underlying approach here,
// only removing the duplication.
func ephemeralPort() int64 {
	return 30000 + time.Now().UnixNano()%20000
}

// startDaemonOn spawns a real tymuxd bound to addr and waits for its "tymuxd
// listening" stdout line, same signal daemon.ts waits on. token is set as
// TYMUXD_TOKEN in the spawned process's env when non-empty.
// startDaemonWithUDS spawns a real tymuxd on an ephemeral loopback TCP port
// (tymuxd currently always needs a valid TYMUXD_ADDR to bind, even when the
// caller only cares about its UDS listener) with TYMUXD_SOCKET_PATH set to
// a fixed path under a fresh subdirectory of t.TempDir(), mirroring
// startDaemonWithToken's shape (Task 7.2.1a). Returns the socket path (not
// an HTTP addr) for the caller to dial via udsdialer.DialUnixHTTPClient.
//
// The socket lives in a "uds" subdirectory that must not already exist,
// not directly in t.TempDir() itself: crates/tymuxd/src/auth.rs's
// ensure_socket_parent_dir requires the socket's immediate parent be
// either freshly created by tymuxd (and thus owned/moded by tymuxd itself
// at 0700) or already owned by the daemon's own uid at exactly that mode —
// t.TempDir() already exists (created by the testing package itself, at
// 0755) before tymuxd ever runs, which fails that check.
func startDaemonWithUDS(t *testing.T, token string) string {
	t.Helper()
	socketPath := filepath.Join(t.TempDir(), "uds", "tymuxd.sock")
	startDaemonOn(t, fmt.Sprintf("127.0.0.1:%d", ephemeralPort()), token, "TYMUXD_SOCKET_PATH="+socketPath)
	return socketPath
}

func startDaemonOn(t *testing.T, addr, token string, extraEnv ...string) string {
	t.Helper()

	// The bind address (addr, e.g. "0.0.0.0:<port>" for the non-loopback
	// auth harness) is not always a valid outbound *connect* target — on
	// Linux the kernel happens to route a 0.0.0.0-destination dial to
	// localhost, but that's not portable (confirmed against
	// crates/tymuxd/src/main.rs's own non-loopback test harness,
	// spawn_non_loopback_test_server, and crates/tymuxd/tests/
	// daemon_startup.rs: both bind 0.0.0.0 but connect via 127.0.0.1
	// explicitly for this reason). Bind stays whatever addr says (needed
	// to exercise the non-loopback auth gate); only the returned,
	// client-facing dial target is normalized to 127.0.0.1.
	connectHost, port, err := net.SplitHostPort(addr)
	if err != nil {
		t.Fatalf("SplitHostPort(%q): %v", addr, err)
	}
	if connectHost == "0.0.0.0" {
		connectHost = "127.0.0.1"
	}
	connectAddr := net.JoinHostPort(connectHost, port)

	stateDir := t.TempDir()

	cmd := exec.Command(resolveBinary(t))
	env := append(os.Environ(), "TYMUXD_ADDR="+addr, "XDG_STATE_HOME="+stateDir)
	if token != "" {
		env = append(env, "TYMUXD_TOKEN="+token)
	}
	env = append(env, extraEnv...)
	cmd.Env = env
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
		waited := make(chan struct{})
		go func() {
			_ = cmd.Wait()
			close(waited)
		}()
		select {
		case <-waited:
		case <-time.After(10 * time.Second):
			// tonic's graceful serve_with_shutdown waits out any
			// still-open streaming RPC before the process actually
			// exits — a test that fails mid-stream (leaving a
			// connection dangling until its own deferred cancel runs)
			// must not turn into a full go-test-timeout hang on top of
			// its real failure. SIGKILL here is a safety net, not the
			// expected path.
			_ = cmd.Process.Kill()
			<-waited
		}
	})

	ready := make(chan struct{})
	go func() {
		// Keep draining stdout for the daemon's whole lifetime, not just
		// until the "listening" line: os/exec's StdoutPipe docs warn that
		// Wait "will close the pipe after seeing the command exit", so
		// reads must keep up throughout, not stop early — otherwise once
		// tracing output past the readiness line fills the OS pipe buffer
		// (64 KiB on Linux), the daemon blocks on its next stdout write
		// and t.Cleanup's Signal+Wait hangs forever waiting for a process
		// that can never finish exiting. Epic 5.2's attach/resume tests
		// (unlike the pre-existing unary-RPC-only test) produce enough
		// attach/input/flood tracing output to hit this in practice.
		var closeOnce sync.Once
		scanner := bufio.NewScanner(stdout)
		for scanner.Scan() {
			if strings.Contains(scanner.Text(), "tymuxd listening") {
				closeOnce.Do(func() { close(ready) })
			}
		}
	}()

	select {
	case <-ready:
	case <-time.After(5 * time.Second):
		t.Fatal("tymuxd did not report listening within 5s")
	}

	return "http://" + connectAddr
}

// newClient mirrors examples/list-sessions/main.go's newClient: tymuxd is a
// strict gRPC server (tonic) on plain h2c, no TLS. token is attached via
// authinterceptor.Interceptor on every outgoing call (unary and streaming
// alike); an empty token is a no-op, matching the loopback/no-auth daemon
// most existing tests here run against.
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

// TestListSessionsReflectsCreateSession mirrors
// clients/ts/test/integration.test.ts's "listSessions reflects a session
// created via createSession" (Story 7.2 AC1): a real unary RPC round-trip
// through the generated client against a live daemon.
func TestListSessionsReflectsCreateSession(t *testing.T) {
	addr := startDaemon(t)
	client := newClient(addr, "")
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

// TestListSessionsRejectsMissingOrWrongToken proves clients/go's
// authinterceptor.Interceptor actually gates a unary call against a live,
// token-gated tymuxd (Task 3.1.1d, AC1) — not just that the interceptor
// compiles or that a loopback daemon still works unauthenticated. An empty
// token (authinterceptor.Interceptor{Token: ""} is a documented no-op, so
// this doubles as the "missing" case) and a wrong token must both fail with
// connect.CodeUnauthenticated.
func TestListSessionsRejectsMissingOrWrongToken(t *testing.T) {
	addr := startDaemonWithToken(t, "s3cr3t")
	ctx := context.Background()

	for name, token := range map[string]string{
		"missing token": "",
		"wrong token":   "wrong-value",
	} {
		t.Run(name, func(t *testing.T) {
			client := newClient(addr, token)
			_, err := client.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
			if err == nil {
				t.Fatal("ListSessions: expected an error, got nil")
			}
			if got := connect.CodeOf(err); got != connect.CodeUnauthenticated {
				t.Fatalf("ListSessions: got code %v, want %v (err: %v)", got, connect.CodeUnauthenticated, err)
			}
		})
	}
}

// TestListSessionsSucceedsWithCorrectToken proves the correct token
// authenticates successfully against a live, token-gated tymuxd (Task
// 3.1.1d, AC2).
func TestListSessionsSucceedsWithCorrectToken(t *testing.T) {
	addr := startDaemonWithToken(t, "s3cr3t")
	client := newClient(addr, "s3cr3t")
	ctx := context.Background()

	if _, err := client.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{})); err != nil {
		t.Fatalf("ListSessions: %v", err)
	}
}

// TestAttachRejectsMissingOrWrongToken proves
// authinterceptor.Interceptor.WrapStreamingClient — not just WrapUnary — is
// actually wired into the Go client's Attach call (Task 3.1.1e, AC3).
// connect-go's convenience connect.UnaryInterceptorFunc only implements
// WrapUnary, leaving WrapStreamingClient a documented no-op that would
// silently exempt Attach from auth (research/pitfalls.md §4); this test is
// the one that would catch that regression. The rejection happens before
// any handler runs, so it can surface on the initial Send (request headers
// rejected outright) or on the first Receive (headers accepted, response
// arrives as a trailers-only Unauthenticated status) depending on
// transport timing — both are checked.
func TestAttachRejectsMissingOrWrongToken(t *testing.T) {
	addr := startDaemonWithToken(t, "s3cr3t")

	for name, token := range map[string]string{
		"missing token": "",
		"wrong token":   "wrong-value",
	} {
		t.Run(name, func(t *testing.T) {
			client := newClient(addr, token)
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()

			stream := client.Attach(ctx)
			err := stream.Send(&tymuxv1.AttachRequest{
				Payload: &tymuxv1.AttachRequest_PaneId{PaneId: "irrelevant-pane-id"},
			})
			if err == nil {
				_, err = stream.Receive()
			}
			if err == nil {
				t.Fatal("Attach: expected an error, got nil")
			}
			if got := connect.CodeOf(err); got != connect.CodeUnauthenticated {
				t.Fatalf("Attach: got code %v, want %v (err: %v)", got, connect.CodeUnauthenticated, err)
			}
		})
	}
}

// TestAttachSucceedsWithCorrectToken proves Attach streams normally when
// the correct token is presented against a live, token-gated tymuxd (Task
// 3.1.1e, AC4).
func TestAttachSucceedsWithCorrectToken(t *testing.T) {
	addr := startDaemonWithToken(t, "s3cr3t")
	client := newClient(addr, "s3cr3t")
	ctx := context.Background()

	session, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{Name: "go-integration-auth-attach"}))
	if err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	paneID := firstPaneID(t, session.Msg)

	// resume_from_seq=Some(0) opts this attach into the OutputChunk field
	// runAttachUntilMarker reads from — see its doc comment and the
	// byte-identical resume test above for why omitting it entirely would
	// leave this call watching only the legacy, seq-less Output field and
	// hang until the timeout.
	const marker = "AUTH-ATTACH-DONE"
	out := runAttachUntilMarker(t, client, paneID, seqPtr(0), "printf '"+marker+"\\n'\n", marker, 10*time.Second)
	if len(out) == 0 {
		t.Fatal("Attach: expected non-empty output, got none")
	}
}

// newUDSClient mirrors newClient but dials socketPath over a real Unix
// domain socket via udsdialer.DialUnixHTTPClient instead of TCP loopback.
// baseURL is a fixed placeholder ("http://unix", the same value
// udsdialer_test.go and examples/list-sessions/main.go use) rather than a
// real address: DialUnixHTTPClient's http2.Transport ignores the
// network/addr connect-go passes at request time and always dials
// socketPath instead (the seam research/stack.md §4 identified), so the
// base URL only needs to be well-formed, not reachable.
func newUDSClient(socketPath string) tymuxv1connect.TymuxServiceClient {
	return tymuxv1connect.NewTymuxServiceClient(udsdialer.DialUnixHTTPClient(socketPath), "http://unix", connect.WithGRPC())
}

// TestListSessionsSucceedsOverUDS is Task 7.3.1a: proves Go's UDS-first
// dialing (Epic 7.1, commit 861e123) actually round-trips against a real,
// dual-listener tymuxd (commit b44aae1) — not just the in-process fake
// server udsdialer_test.go's own TestDialUnixHTTPClientRoundTripsListSessions
// already covers, and not just that this uid (the daemon's own) is
// authorized, which mirrors clients/ts's and tymux-cli's own accept-path
// integration tests (validation.md R13 row).
func TestListSessionsSucceedsOverUDS(t *testing.T) {
	socketPath := startDaemonWithUDS(t, "")
	client := newUDSClient(socketPath)
	ctx := context.Background()

	created, err := client.CreateSession(ctx, connect.NewRequest(&tymuxv1.CreateSessionRequest{Name: "go-integration-uds"}))
	if err != nil {
		t.Fatalf("CreateSession over UDS: %v", err)
	}

	listed, err := client.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	if err != nil {
		t.Fatalf("ListSessions over UDS: %v", err)
	}

	var found bool
	for _, s := range listed.Msg.GetSessions() {
		if s.GetId() == created.Msg.GetId() {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("created session %q not found in ListSessions response over UDS", created.Msg.GetId())
	}
}

// TestHelperProcessUDSListSessionsRejected is a re-exec helper, not a real
// test: TestListSessionsRejectsOverUDSWithMismatchedUID below spawns the Go
// test binary itself (via os.Args[0]) with -test.run scoped to just this
// function so it runs as a distinct OS process it can pin to a specific uid
// via exec.Cmd.SysProcAttr.Credential — the same re-exec-self idiom
// os/exec_test.go's own TestHelperProcess uses, needed here because Go has
// no in-process way to change a goroutine's real/effective uid mid-test.
// The GO_WANT_HELPER_PROCESS guard mirrors that idiom too, so a plain `go
// test -run TestHelperProcessUDSListSessionsRejected` (no env var) is a
// harmless skip rather than a confusing failure.
func TestHelperProcessUDSListSessionsRejected(t *testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		t.Skip("only runs as a re-exec helper for TestListSessionsRejectsOverUDSWithMismatchedUID")
	}
	socketPath := os.Getenv("TYMUX_HELPER_SOCKET_PATH")
	client := newUDSClient(socketPath)
	_, err := client.ListSessions(context.Background(), connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	// Printed (not returned/asserted here) because this process's exit
	// code/panic output is not what the parent test inspects — it greps
	// this exact, unambiguous line out of the child's combined output.
	fmt.Printf("HELPER-RESULT:%s\n", connect.CodeOf(err))
}

// TestListSessionsRejectsOverUDSWithMismatchedUID is Task 7.3.1b: the true
// cross-uid reject proof validation.md's R14 row calls for — a real
// SO_PEERCRED-delivered uid mismatch rejected by crates/tymuxd/src/auth.rs's
// UdsPeerCredInterceptor with Status::permission_denied (connect-go's
// connect.CodePermissionDenied), not just the synthetic UCred Story 3.1.2's
// peer_is_authorized unit tests already exercise.
//
// Per pitfalls.md §7 / validation.md R14 / plan.md's Unresolved Questions,
// this is only meaningful when the test process can actually create a
// second, genuinely different real uid — which requires CAP_SETUID
// (root, in practice) and is therefore unavailable on this repo's actual
// CI (.github/workflows/ci.yml runs plain ubuntu-latest/macos-latest, no
// container, no root). This test skips unconditionally in that
// environment rather than failing; it is not conditionally gated on a
// feature flag, only on privilege.
//
// The daemon binds its UDS at mode 0600 (bind_uds_listener's default, no
// --socket-group configured), and man 7 unix documents that "connecting to
// a stream socket object requires write permission on that socket" — so a
// mismatched-uid connect() only reaches the daemon's accept()/peer_cred
// gate at all (rather than failing earlier with a plain filesystem EACCES,
// which is Task 7.3.1c's distinct scenario) when the connecting process
// holds CAP_DAC_OVERRIDE, i.e. is root. That is why, once privilege is
// available, this test spawns the *daemon* under a fixed unprivileged uid
// ("nobody") and keeps the client at uid 0: root's DAC-bypass is what lets
// its connect() succeed despite the mismatch, so the rejection actually
// observed is the daemon's own peer_is_authorized decision (peer uid 0 !=
// daemon uid, no --socket-group configured), not a filesystem-level error.
func TestListSessionsRejectsOverUDSWithMismatchedUID(t *testing.T) {
	if os.Geteuid() != 0 {
		t.Skip("requires root/CAP_SETUID to spawn the daemon under a distinct unprivileged uid and reach a real SO_PEERCRED mismatch (see plan.md Unresolved Questions and validation.md R14) -- not available on this repo's actual ubuntu-latest/macos-latest CI runners")
	}

	nobody, err := user.Lookup("nobody")
	if err != nil {
		t.Skipf("could not resolve the 'nobody' user needed to run the daemon under a distinct unprivileged uid: %v", err)
	}
	daemonUID, err := strconv.ParseUint(nobody.Uid, 10, 32)
	if err != nil {
		t.Fatalf("parsing nobody's uid %q: %v", nobody.Uid, err)
	}
	daemonGID, err := strconv.ParseUint(nobody.Gid, 10, 32)
	if err != nil {
		t.Fatalf("parsing nobody's gid %q: %v", nobody.Gid, err)
	}

	// The socket's immediate parent directory must not already exist —
	// crates/tymuxd/src/auth.rs's ensure_socket_parent_dir requires it be
	// either freshly created (and thus owned/moded by tymuxd itself, here
	// running as "nobody") or already owned by the daemon's own uid at
	// its expected mode; a directory this (root) test process pre-created
	// would fail that check. The grandparent, which this process does
	// pre-create, is chmoded world-searchable so "nobody" can still
	// traverse into it to create its own socket directory underneath.
	base, err := os.MkdirTemp("", "tymuxd-uds-reject-test-*")
	if err != nil {
		t.Fatalf("MkdirTemp: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(base) })
	if err := os.Chmod(base, 0o711); err != nil {
		t.Fatalf("chmod %s: %v", base, err)
	}
	socketPath := filepath.Join(base, "sockdir", "tymuxd.sock")

	stateDir := filepath.Join(base, "state")
	if err := os.MkdirAll(stateDir, 0o755); err != nil {
		t.Fatalf("MkdirAll %s: %v", stateDir, err)
	}
	if err := os.Chown(stateDir, int(daemonUID), int(daemonGID)); err != nil {
		t.Fatalf("chown %s to nobody: %v", stateDir, err)
	}

	addr := fmt.Sprintf("127.0.0.1:%d", ephemeralPort())
	daemonCmd := exec.Command(resolveBinary(t))
	daemonCmd.Env = append(os.Environ(),
		"TYMUXD_ADDR="+addr,
		"XDG_STATE_HOME="+stateDir,
		"TYMUXD_SOCKET_PATH="+socketPath,
	)
	daemonCmd.SysProcAttr = &syscall.SysProcAttr{
		Credential: &syscall.Credential{Uid: uint32(daemonUID), Gid: uint32(daemonGID)},
	}
	stdout, err := daemonCmd.StdoutPipe()
	if err != nil {
		t.Fatalf("StdoutPipe: %v", err)
	}
	daemonCmd.Stderr = os.Stderr
	if err := daemonCmd.Start(); err != nil {
		t.Fatalf("tymuxd start (as nobody): %v", err)
	}
	t.Cleanup(func() {
		_ = daemonCmd.Process.Signal(os.Interrupt)
		_ = daemonCmd.Wait()
	})

	ready := make(chan struct{})
	go func() {
		var closeOnce sync.Once
		scanner := bufio.NewScanner(stdout)
		for scanner.Scan() {
			if strings.Contains(scanner.Text(), "tymuxd listening") {
				closeOnce.Do(func() { close(ready) })
			}
		}
	}()
	select {
	case <-ready:
	case <-time.After(5 * time.Second):
		t.Fatal("tymuxd (as nobody) did not report listening within 5s")
	}

	// Re-exec this test binary as the "client" half, pinned to uid 0
	// (root) via Credential — see the doc comment above for why staying
	// root, not "nobody" or any other non-daemon uid, is what lets this
	// child's connect() actually reach the daemon's peer_cred gate.
	clientCmd := exec.Command(os.Args[0], "-test.run=^TestHelperProcessUDSListSessionsRejected$", "-test.v")
	clientCmd.Env = append(os.Environ(),
		"GO_WANT_HELPER_PROCESS=1",
		"TYMUX_HELPER_SOCKET_PATH="+socketPath,
	)
	clientCmd.SysProcAttr = &syscall.SysProcAttr{
		Credential: &syscall.Credential{Uid: 0, Gid: 0},
	}
	out, err := clientCmd.CombinedOutput()
	if err != nil {
		t.Fatalf("client helper subprocess failed: %v\noutput:\n%s", err, out)
	}
	wantLine := "HELPER-RESULT:" + connect.CodePermissionDenied.String()
	if !strings.Contains(string(out), wantLine) {
		t.Fatalf("expected client helper subprocess output to contain %q, got:\n%s", wantLine, out)
	}
}
