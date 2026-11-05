package liveness

import (
	"context"
	"errors"
	"net"
	"os"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

type prober struct {
	err   error
	hook  func(ctx context.Context) error
	calls int
}

func (p *prober) GetServerInfo(ctx context.Context, _ *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error) {
	p.calls++
	err := p.err
	if p.hook != nil {
		err = p.hook(ctx)
	}
	if err != nil {
		return nil, err
	}
	return connect.NewResponse(&v1.ServerInfo{}), nil
}

func dialOK(context.Context, string, string, time.Duration) (net.Conn, error) {
	c, _ := net.Pipe()
	return c, nil
}

func dialFail(err error) func(context.Context, string, string, time.Duration) (net.Conn, error) {
	return func(context.Context, string, string, time.Duration) (net.Conn, error) { return nil, err }
}

func opErr(errno syscall.Errno) error {
	return &net.OpError{Op: "dial", Err: os.NewSyscallError("connect", errno)}
}

func TestClassification(t *testing.T) {
	t.Parallel()
	cancelled, cancel := context.WithCancel(context.Background())
	cancel()
	tests := []struct {
		name    string
		network string
		dial    func(context.Context, string, string, time.Duration) (net.Conn, error)
		p       *prober
		ctx     context.Context
		want    Result
		usable  bool
		probed  bool
	}{
		{"healthy", "tcp", dialOK, &prober{}, context.Background(), Healthy, true, true},
		{"refused tcp", "tcp", dialFail(opErr(syscall.ECONNREFUSED)), &prober{}, context.Background(), NotListening, false, false},
		{"refused unix", "unix", dialFail(opErr(syscall.ECONNREFUSED)), &prober{}, context.Background(), NotListening, false, false},
		{"enoent unix is down", "unix", dialFail(opErr(syscall.ENOENT)), &prober{}, context.Background(), NotListening, false, false},
		{"enoent tcp is not proof", "tcp", dialFail(opErr(syscall.ENOENT)), &prober{}, context.Background(), Busy, true, false},
		{"fd exhaustion is busy", "tcp", dialFail(opErr(syscall.EMFILE)), &prober{}, context.Background(), Busy, true, false},
		{"dial timeout is busy", "tcp", dialFail(context.DeadlineExceeded), &prober{}, context.Background(), Busy, true, false},
		{"rpc deadline is busy", "tcp", dialOK, &prober{err: connect.NewError(connect.CodeDeadlineExceeded, errors.New("slow"))}, context.Background(), Busy, true, true},
		{"non-grpc reply is unhealthy", "tcp", dialOK, &prober{err: connect.NewError(connect.CodeUnknown, errors.New("http/1.1 banner"))}, context.Background(), Unhealthy, false, true},
		{"unauthenticated reply is access denied", "tcp", dialOK, &prober{err: connect.NewError(connect.CodeUnauthenticated, errors.New("no token"))}, context.Background(), AccessDenied, false, true},
		{"permission denied reply is access denied", "unix", dialOK, &prober{err: connect.NewError(connect.CodePermissionDenied, errors.New("uid rejected"))}, context.Background(), AccessDenied, false, true},
		{"untrusted socket dial is access denied, not busy", "unix", dialFail(untrustedDialErr{}), &prober{}, context.Background(), AccessDenied, false, false},
		{"cancelled during dial is not 'down'", "tcp", dialFail(context.Canceled), &prober{}, cancelled, Unhealthy, false, false},
		{"cancelled during rpc is not 'down'", "tcp", dialOK, &prober{err: context.Canceled}, cancelled, Unhealthy, false, true},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got := Probe(tc.ctx, tc.network, "addr", tc.p, Options{Dial: tc.dial})
			if got.Alive() != (tc.usable || tc.want == AccessDenied) {
				t.Fatalf("%s: Alive() = %v", got, got.Alive())
			}
			if got != tc.want || got.Usable() != tc.usable {
				t.Fatalf("got %s (usable=%v), want %s (usable=%v)", got, got.Usable(), tc.want, tc.usable)
			}
			if (tc.p.calls == 1) != tc.probed {
				t.Fatalf("GetServerInfo calls = %d, want probed=%v", tc.p.calls, tc.probed)
			}
		})
	}
}

func TestProbeTimeoutBoundsRPCAndReportsBusy(t *testing.T) {
	t.Parallel()
	p := &prober{hook: func(ctx context.Context) error { <-ctx.Done(); return ctx.Err() }}
	if got := Probe(context.Background(), "tcp", "a", p, Options{Dial: dialOK, ProbeTimeout: time.Millisecond}); got != Busy {
		t.Fatalf("got %s, want busy", got)
	}
}

func TestStringIsBoundsChecked(t *testing.T) {
	t.Parallel()
	for _, r := range []Result{-1, 99} {
		if r.String() == "" {
			t.Fatal("empty String for out-of-range Result")
		}
	}
	if AccessDenied.String() != "access-denied" {
		t.Fatalf("got %q", AccessDenied.String())
	}
}

type untrustedDialErr struct{}

func (untrustedDialErr) Error() string      { return "socket not trusted" }
func (untrustedDialErr) AccessDenied() bool { return true }
