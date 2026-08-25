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
	"sync"
	"testing"
	"time"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
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
	client := newClient(addr)
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
	client := newClient(addr)
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
