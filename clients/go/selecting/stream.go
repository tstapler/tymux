package selecting

import (
	"context"
	"errors"
	"io"
	"sync"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// Attach opens the stream on the current transport. A refused dial is reported to the
// selector from the stream's first Send/Receive, so the next Attach (e.g. a reconnect
// loop's redial) lands on the fallback. The failed stream itself is not retried. A failover
// check may block that first Send/Receive for up to ProbeTimeout (bounded by ctx).
func (c *Client) Attach(ctx context.Context) AttachStream {
	if c.noTransport() {
		return errStream{ErrNoTransport}
	}
	t, kind := c.pick(ctx)
	return &failoverStream{inner: t.Attach(ctx), c: c, kind: kind, ctx: ctx}
}

type failoverStream struct {
	inner AttachStream
	c     *Client
	kind  Kind
	ctx   context.Context //nolint:containedctx // the Attach ctx bounds the failover probe

	mu        sync.Mutex
	rxStarted bool          // a Receive has begun: the stream has a reader, so Send must never Receive
	drained   bool          // the one-time drain already ran
	drainDone chan struct{} // non-nil while a drain's Receive is in flight
	stash     *v1.AttachEvent
}

func (f *failoverStream) observe(err error) error {
	if err != nil {
		f.c.failover(f.ctx, f.kind, err)
	}
	return annotate(f.kind, err)
}

// Send surfaces the real dial error: connect-go reports only io.EOF from Send when the
// request failed, with the cause on Receive, and a bare EOF would hide a refused dial
// from the selector. It drains at most once and only before any Receive has begun:
// Receive is not safe to call concurrently, and on a live stream the reader owns it. An
// event the drain consumes is stashed for the next Receive.
func (f *failoverStream) Send(r *v1.AttachRequest) error {
	err := f.inner.Send(r)
	if errors.Is(err, io.EOF) {
		err = f.drainOnce(err)
	}
	return f.observe(err)
}

func (f *failoverStream) drainOnce(eof error) error {
	f.mu.Lock()
	if f.rxStarted || f.drained {
		f.mu.Unlock()
		return eof
	}
	f.drained = true
	done := make(chan struct{})
	f.drainDone = done
	f.mu.Unlock()

	ev, rxErr := f.inner.Receive()

	f.mu.Lock()
	f.drainDone = nil
	if rxErr == nil {
		f.stash = ev
	}
	f.mu.Unlock()
	close(done)
	if rxErr != nil {
		return rxErr
	}
	return eof // the send still failed; the drained event is kept for Receive
}

func (f *failoverStream) Receive() (*v1.AttachEvent, error) {
	f.mu.Lock()
	f.rxStarted = true
	done := f.drainDone
	f.mu.Unlock()
	if done != nil {
		<-done
	}
	f.mu.Lock()
	if ev := f.stash; ev != nil {
		f.stash = nil
		f.mu.Unlock()
		return ev, nil
	}
	f.mu.Unlock()
	ev, err := f.inner.Receive()
	return ev, f.observe(err)
}

func (f *failoverStream) CloseRequest() error  { return f.inner.CloseRequest() }
func (f *failoverStream) CloseResponse() error { return f.inner.CloseResponse() }

// errStream is the AttachStream of a Client with no transport.
type errStream struct{ err error }

func (s errStream) Send(*v1.AttachRequest) error      { return s.err }
func (s errStream) Receive() (*v1.AttachEvent, error) { return nil, s.err }
func (s errStream) CloseRequest() error               { return nil }
func (s errStream) CloseResponse() error              { return nil }
