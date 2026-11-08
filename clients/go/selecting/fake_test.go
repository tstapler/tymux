package selecting

import (
	"context"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// fakeTransport scripts GetServerInfo (probe) and ListSessions (call) results and counts calls.
type fakeTransport struct {
	Transport // nil: any unscripted RPC panics, which fails the test loudly

	mu         sync.Mutex
	infoErr    error
	listErr    error
	infoCalls  int
	listCalls  int
	infoHook   func(ctx context.Context) error // overrides infoErr when set
	attachWith func(ctx context.Context) AttachStream
	activity   []v1.PaneActivity
}

func (f *fakeTransport) GetServerInfo(ctx context.Context, _ *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error) {
	f.mu.Lock()
	f.infoCalls++
	err, hook := f.infoErr, f.infoHook
	f.mu.Unlock()
	if hook != nil {
		err = hook(ctx)
	}
	if err != nil {
		return nil, err
	}
	return connect.NewResponse(&v1.ServerInfo{}), nil
}

func (f *fakeTransport) ListSessions(context.Context, *connect.Request[v1.ListSessionsRequest]) (*connect.Response[v1.ListSessionsResponse], error) {
	f.mu.Lock()
	f.listCalls++
	err := f.listErr
	f.mu.Unlock()
	if err != nil {
		return nil, err
	}
	return connect.NewResponse(&v1.ListSessionsResponse{}), nil
}

func (f *fakeTransport) SetPaneActivity(_ context.Context, r *connect.Request[v1.SetPaneActivityRequest]) (*connect.Response[v1.SetPaneActivityResponse], error) {
	f.mu.Lock()
	f.activity = append(f.activity, r.Msg.GetActivity())
	f.mu.Unlock()
	return connect.NewResponse(&v1.SetPaneActivityResponse{}), nil
}

func (f *fakeTransport) Attach(ctx context.Context) AttachStream { return f.attachWith(ctx) }

func (f *fakeTransport) set(info, list error) {
	f.mu.Lock()
	f.infoErr, f.listErr = info, list
	f.mu.Unlock()
}

func (f *fakeTransport) calls() (info, list int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.infoCalls, f.listCalls
}

type fakeClock struct{ ns atomic.Int64 }

func newFakeClock() *fakeClock {
	c := &fakeClock{}
	c.ns.Store(time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC).UnixNano())
	return c
}
func (c *fakeClock) Now() time.Time          { return time.Unix(0, c.ns.Load()) }
func (c *fakeClock) Advance(d time.Duration) { c.ns.Add(int64(d)) }

type transition struct {
	From, To Kind
	Why      string
}

type transitionLog struct {
	mu sync.Mutex
	ts []transition
}

func (l *transitionLog) record(from, to Kind, why string) {
	l.mu.Lock()
	l.ts = append(l.ts, transition{from, to, why})
	l.mu.Unlock()
}

func (l *transitionLog) all() []transition {
	l.mu.Lock()
	defer l.mu.Unlock()
	return append([]transition(nil), l.ts...)
}

func list(t *testing.T, c *Client) error {
	t.Helper()
	_, err := c.ListSessions(context.Background(), connect.NewRequest(&v1.ListSessionsRequest{}))
	return err
}
