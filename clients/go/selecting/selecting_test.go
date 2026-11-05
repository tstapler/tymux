package selecting

import (
	"context"
	"errors"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// dialErr mimics what connect-go surfaces for a failed dial: a connect error wrapping a net.OpError.
func dialErr(errno syscall.Errno) error {
	return connect.NewError(connect.CodeUnavailable,
		&net.OpError{Op: "dial", Net: "unix", Err: os.NewSyscallError("connect", errno)})
}

type rig struct {
	c           *Client
	unix, tcp   *fakeTransport
	clock       *fakeClock
	transitions *transitionLog
}

func newRig(opts Options) *rig {
	r := &rig{unix: &fakeTransport{}, tcp: &fakeTransport{}, clock: newFakeClock(), transitions: &transitionLog{}}
	opts.Now = r.clock.Now
	opts.OnTransition = r.transitions.record
	r.c = NewFromTransports(r.unix, r.tcp, opts)
	return r
}

func TestUnixFirstWhenHealthy(t *testing.T) {
	t.Parallel()
	r := newRig(Options{})
	if err := list(t, r.c); err != nil {
		t.Fatal(err)
	}
	if _, n := r.unix.calls(); n != 1 {
		t.Fatalf("unix calls = %d, want 1", n)
	}
	if i, n := r.tcp.calls(); i != 0 || n != 0 {
		t.Fatalf("tcp touched: probes=%d calls=%d", i, n)
	}
	if r.c.Active() != Unix {
		t.Fatalf("active = %s", r.c.Active())
	}
}

func TestFailsOverOnlyOnRefusedDial(t *testing.T) {
	t.Parallel()
	for _, errno := range []syscall.Errno{syscall.ENOENT, syscall.ECONNREFUSED} {
		t.Run(errno.Error(), func(t *testing.T) {
			t.Parallel()
			r := newRig(Options{})
			r.unix.set(dialErr(errno), dialErr(errno))
			if err := list(t, r.c); err != nil {
				t.Fatalf("want success via tcp, got %v", err)
			}
			if _, n := r.tcp.calls(); n != 1 {
				t.Fatalf("tcp list calls = %d, want 1", n)
			}
			if r.c.Active() != TCP {
				t.Fatalf("active = %s", r.c.Active())
			}
			got := r.transitions.all()
			if len(got) != 1 || got[0].From != Unix || got[0].To != TCP || got[0].Why == "" {
				t.Fatalf("transitions = %+v", got)
			}
		})
	}
}

func TestNoRetryOnNonRefusedErrors(t *testing.T) {
	t.Parallel()
	for name, err := range map[string]error{
		"reset":       dialErr(syscall.ECONNRESET),
		"epipe":       dialErr(syscall.EPIPE),
		"timeout":     connect.NewError(connect.CodeDeadlineExceeded, context.DeadlineExceeded),
		"application": connect.NewError(connect.CodeNotFound, errors.New("no such session")),
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()
			r := newRig(Options{})
			r.unix.set(nil, err)
			if got := list(t, r.c); got == nil {
				t.Fatal("want the error back")
			}
			if _, n := r.unix.calls(); n != 1 {
				t.Fatalf("unix calls = %d, want exactly 1 (never replay)", n)
			}
			if i, n := r.tcp.calls(); i != 0 || n != 0 {
				t.Fatalf("tcp touched: probes=%d calls=%d", i, n)
			}
		})
	}
}

func TestEACCESIsHardErrorNeverFallsBack(t *testing.T) {
	t.Parallel()
	r := newRig(Options{})
	r.unix.set(dialErr(syscall.EACCES), dialErr(syscall.EACCES))
	err := list(t, r.c)
	if !errors.Is(err, ErrSocketAccessDenied) || !errors.Is(err, syscall.EACCES) {
		t.Fatalf("want ErrSocketAccessDenied wrapping EACCES, got %v", err)
	}
	if i, n := r.tcp.calls(); i != 0 || n != 0 {
		t.Fatalf("EACCES fell back to tcp: probes=%d calls=%d", i, n)
	}
	if r.c.Active() != Unix || len(r.transitions.all()) != 0 {
		t.Fatalf("selector moved on EACCES: %s %+v", r.c.Active(), r.transitions.all())
	}
}

func TestRetriesAtMostOnce(t *testing.T) {
	t.Parallel()
	r := newRig(Options{})
	r.unix.set(dialErr(syscall.ECONNREFUSED), dialErr(syscall.ECONNREFUSED))
	r.tcp.set(nil, dialErr(syscall.ECONNREFUSED)) // probe passes, the real call is refused too
	if err := list(t, r.c); !IsDialRefused(err) {
		t.Fatalf("want the refused error, got %v", err)
	}
	if _, n := r.unix.calls(); n != 1 {
		t.Fatalf("unix calls = %d, want 1", n)
	}
	if _, n := r.tcp.calls(); n != 1 {
		t.Fatalf("tcp calls = %d, want 1 (no second retry)", n)
	}
}

func TestNoFailoverWhenFallbackProbeFails(t *testing.T) {
	t.Parallel()
	r := newRig(Options{})
	r.unix.set(nil, dialErr(syscall.ENOENT))
	r.tcp.set(dialErr(syscall.ECONNREFUSED), nil)
	if err := list(t, r.c); !IsDialRefused(err) {
		t.Fatalf("want original refused error, got %v", err)
	}
	if _, n := r.tcp.calls(); n != 0 {
		t.Fatalf("tcp call made without a passing probe")
	}
	if r.c.Active() != Unix {
		t.Fatalf("active = %s", r.c.Active())
	}
}

func fallBackToTCP(t *testing.T, r *rig) {
	t.Helper()
	r.unix.set(dialErr(syscall.ENOENT), dialErr(syscall.ENOENT))
	if err := list(t, r.c); err != nil || r.c.Active() != TCP {
		t.Fatalf("setup: err=%v active=%s", err, r.c.Active())
	}
}

func TestStickyThenReprobeReturnsToUnix(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: 30 * time.Second})
	fallBackToTCP(t, r)
	probesBefore, _ := r.unix.calls()

	r.clock.Advance(29 * time.Second)
	if err := list(t, r.c); err != nil {
		t.Fatal(err)
	}
	if p, _ := r.unix.calls(); p != probesBefore {
		t.Fatalf("unix probed before the interval elapsed")
	}

	r.unix.set(nil, nil) // socket is back
	r.clock.Advance(2 * time.Second)
	if err := list(t, r.c); err != nil {
		t.Fatal(err)
	}
	if r.c.Active() != Unix {
		t.Fatalf("active = %s, want unix after re-probe", r.c.Active())
	}
	got := r.transitions.all()
	if last := got[len(got)-1]; last.From != TCP || last.To != Unix {
		t.Fatalf("transitions = %+v", got)
	}
}

func TestFailedReprobeBacksOffAnotherInterval(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: 30 * time.Second})
	fallBackToTCP(t, r)
	base, _ := r.unix.calls()

	r.clock.Advance(31 * time.Second)
	_ = list(t, r.c) // re-probe fails (still ENOENT), stays on tcp
	if p, _ := r.unix.calls(); p != base+1 {
		t.Fatalf("probes = %d, want %d", p, base+1)
	}
	if r.c.Active() != TCP {
		t.Fatal("left tcp after a failed probe")
	}
	r.clock.Advance(10 * time.Second)
	_ = list(t, r.c)
	if p, _ := r.unix.calls(); p != base+1 {
		t.Fatalf("re-probed again inside the backoff window: %d", p)
	}
}

func TestReprobeClaimedByExactlyOneCaller(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: 30 * time.Second})
	fallBackToTCP(t, r)
	base, _ := r.unix.calls()

	release := make(chan struct{})
	entered := make(chan struct{}, 1)
	r.unix.mu.Lock()
	var first atomic.Bool
	r.unix.infoHook = func(context.Context) error {
		if first.CompareAndSwap(false, true) {
			entered <- struct{}{}
			<-release
		}
		return dialErr(syscall.ENOENT)
	}
	r.unix.mu.Unlock()
	r.clock.Advance(31 * time.Second)

	var wg sync.WaitGroup
	wg.Add(1)
	go func() { defer wg.Done(); _ = list(t, r.c) }()
	<-entered // the claiming caller is inside its probe

	for range 8 { // everyone else must skip the probe and use tcp without blocking
		if err := list(t, r.c); err != nil {
			t.Fatal(err)
		}
	}
	close(release)
	wg.Wait()
	if p, _ := r.unix.calls(); p != base+1 {
		t.Fatalf("unix probes = %d, want exactly one claimed probe (%d)", p, base+1)
	}
}

func TestReprobeEACCESSwitchesToUnixSoNextCallSurfacesIt(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: time.Second})
	fallBackToTCP(t, r)
	r.unix.set(dialErr(syscall.EACCES), dialErr(syscall.EACCES))
	r.clock.Advance(2 * time.Second)
	err := list(t, r.c)
	if !errors.Is(err, ErrSocketAccessDenied) {
		t.Fatalf("want hard EACCES error, got %v", err)
	}
	if r.c.Active() != Unix {
		t.Fatalf("active = %s", r.c.Active())
	}
}

func TestCancelledCallerReturnsReprobeClaim(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: 30 * time.Second})
	fallBackToTCP(t, r)
	r.unix.mu.Lock()
	r.unix.infoHook = func(ctx context.Context) error { return ctx.Err() }
	r.unix.mu.Unlock()
	r.clock.Advance(31 * time.Second)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	_, _ = r.c.ListSessions(ctx, connect.NewRequest(&v1.ListSessionsRequest{}))
	base, _ := r.unix.calls()

	r.unix.set(nil, nil)
	r.unix.mu.Lock()
	r.unix.infoHook = nil
	r.unix.mu.Unlock()
	if err := list(t, r.c); err != nil {
		t.Fatal(err)
	}
	if p, _ := r.unix.calls(); p != base+1 || r.c.Active() != Unix {
		t.Fatalf("claim was not returned: probes=%d active=%s", p, r.c.Active())
	}
}

func TestProbeIsBoundedByCallerContext(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ProbeTimeout: time.Hour}) // only the caller's ctx can end it
	r.unix.set(nil, dialErr(syscall.ENOENT))
	r.tcp.mu.Lock()
	r.tcp.infoHook = func(ctx context.Context) error { <-ctx.Done(); return ctx.Err() }
	r.tcp.mu.Unlock()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	done := make(chan struct{})
	go func() {
		_, _ = r.c.ListSessions(ctx, connect.NewRequest(&v1.ListSessionsRequest{}))
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("probe ignored the caller's cancelled ctx")
	}
}

func TestQuietModeSuppressesCallbackAndWarn(t *testing.T) {
	t.Parallel()
	lg := &recordingLogger{}
	r := newRig(Options{Quiet: true, Logger: lg})
	r.unix.set(dialErr(syscall.ENOENT), dialErr(syscall.ENOENT))
	if err := list(t, r.c); err != nil || r.c.Active() != TCP {
		t.Fatalf("err=%v active=%s", err, r.c.Active())
	}
	if n := len(r.transitions.all()); n != 0 {
		t.Fatalf("quiet client invoked OnTransition %d times", n)
	}
	if lg.warns != 0 || lg.debugs != 1 {
		t.Fatalf("warns=%d debugs=%d", lg.warns, lg.debugs)
	}
}

func TestLoudModeWarnsOnTransition(t *testing.T) {
	t.Parallel()
	lg := &recordingLogger{}
	r := newRig(Options{Logger: lg})
	fallBackToTCP(t, r)
	if lg.warns != 1 {
		t.Fatalf("warns = %d", lg.warns)
	}
}

func TestNilTCPDisablesFallbackAndNilUnixPinsTCP(t *testing.T) {
	t.Parallel()
	u := &fakeTransport{listErr: dialErr(syscall.ENOENT)}
	c := NewFromTransports(u, nil, Options{})
	if err := list(t, c); !IsDialRefused(err) {
		t.Fatalf("got %v", err)
	}
	tc := &fakeTransport{}
	c2 := NewFromTransports(nil, tc, Options{})
	if err := list(t, c2); err != nil || c2.Active() != TCP {
		t.Fatalf("err=%v active=%s", err, c2.Active())
	}
}

type recordingLogger struct {
	mu            sync.Mutex
	warns, debugs int
}

func (l *recordingLogger) Debug(string, ...any) { l.mu.Lock(); l.debugs++; l.mu.Unlock() }
func (l *recordingLogger) Warn(string, ...any)  { l.mu.Lock(); l.warns++; l.mu.Unlock() }

func TestReprobePermissionDeniedSwitchesToUnix(t *testing.T) {
	t.Parallel()
	for _, code := range []connect.Code{connect.CodePermissionDenied, connect.CodeUnauthenticated} {
		r := newRig(Options{ReprobeInterval: time.Second})
		fallBackToTCP(t, r)
		r.unix.set(connect.NewError(code, errors.New("rejected")), nil)
		r.clock.Advance(2 * time.Second)
		_ = list(t, r.c)
		if r.c.Active() != Unix {
			t.Fatalf("%s: stayed on tcp after the daemon rejected us", code)
		}
	}
}

func TestNewFromTransportsRequiresOne(t *testing.T) {
	t.Parallel()
	defer func() {
		if recover() == nil {
			t.Fatal("want panic")
		}
	}()
	NewFromTransports(nil, nil, Options{})
}
