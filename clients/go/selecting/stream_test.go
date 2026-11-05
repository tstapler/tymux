package selecting

import (
	"context"
	"io"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// fakeStream mimics connect-go's bidi stream on a failed dial: Send reports io.EOF and the
// real cause arrives on Receive. It fails the test if Receive is ever entered concurrently.
type fakeStream struct {
	t         *testing.T
	sendErr   error
	recv      func(n int) (*v1.AttachEvent, error)
	recvCalls atomic.Int32
	inRecv    atomic.Int32
	sendCalls atomic.Int32
}

func (s *fakeStream) Send(*v1.AttachRequest) error { s.sendCalls.Add(1); return s.sendErr }
func (s *fakeStream) Receive() (*v1.AttachEvent, error) {
	if s.inRecv.Add(1) != 1 {
		s.t.Error("two concurrent Receive calls")
	}
	defer s.inRecv.Add(-1)
	n := int(s.recvCalls.Add(1))
	return s.recv(n)
}
func (s *fakeStream) CloseRequest() error  { return nil }
func (s *fakeStream) CloseResponse() error { return nil }

func attachRig(t *testing.T, st *fakeStream) (*rig, AttachStream) {
	t.Helper()
	r := newRig(Options{})
	st.t = t
	r.unix.attachWith = func(context.Context) AttachStream { return st }
	return r, r.c.Attach(context.Background())
}

func TestAttachRefusedDialSurfacesRealErrorFromSendAndFailsOver(t *testing.T) {
	t.Parallel()
	st := &fakeStream{sendErr: io.EOF, recv: func(int) (*v1.AttachEvent, error) { return nil, dialErr(syscall.ECONNREFUSED) }}
	r, s := attachRig(t, st)
	err := s.Send(&v1.AttachRequest{})
	if !refusedForTest(err) {
		t.Fatalf("Send = %v, want the refused dial, not a bare EOF", err)
	}
	if r.c.Active() != TCP {
		t.Fatalf("active = %s, want tcp after a refused Attach dial", r.c.Active())
	}
}

func TestAttachDrainsAtMostOnce(t *testing.T) {
	t.Parallel()
	st := &fakeStream{sendErr: io.EOF, recv: func(int) (*v1.AttachEvent, error) { return nil, io.EOF }}
	_, s := attachRig(t, st)
	for range 5 {
		_ = s.Send(&v1.AttachRequest{})
	}
	if n := st.recvCalls.Load(); n != 1 {
		t.Fatalf("Receive called %d times from Send, want 1", n)
	}
}

func TestAttachNeverDrainsAfterReceiveStarted(t *testing.T) {
	t.Parallel()
	st := &fakeStream{sendErr: io.EOF, recv: func(int) (*v1.AttachEvent, error) { return nil, io.EOF }}
	_, s := attachRig(t, st)
	_, _ = s.Receive()
	before := st.recvCalls.Load()
	if err := s.Send(&v1.AttachRequest{}); err != io.EOF {
		t.Fatalf("Send = %v, want plain EOF", err)
	}
	if st.recvCalls.Load() != before {
		t.Fatal("Send drained a stream that already has a reader")
	}
}

func TestAttachDrainedEventIsStashedForNextReceive(t *testing.T) {
	t.Parallel()
	ev := &v1.AttachEvent{}
	st := &fakeStream{sendErr: io.EOF, recv: func(n int) (*v1.AttachEvent, error) {
		if n == 1 {
			return ev, nil
		}
		return nil, io.EOF
	}}
	_, s := attachRig(t, st)
	if err := s.Send(&v1.AttachRequest{}); err != io.EOF {
		t.Fatalf("Send = %v, want EOF (the send still failed)", err)
	}
	got, err := s.Receive()
	if err != nil || got != ev {
		t.Fatalf("Receive = %v, %v; want the stashed event", got, err)
	}
	if st.recvCalls.Load() != 1 {
		t.Fatal("stashed event should not cost an inner Receive")
	}
	if _, err := s.Receive(); err != io.EOF {
		t.Fatalf("second Receive = %v", err)
	}
}

func TestAttachNoConcurrentReceiveWhileReaderOwnsStream(t *testing.T) {
	t.Parallel()
	started := make(chan struct{})
	release := make(chan struct{})
	st := &fakeStream{sendErr: io.EOF, recv: func(int) (*v1.AttachEvent, error) {
		close(started)
		<-release
		return nil, io.EOF
	}}
	_, s := attachRig(t, st)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() { defer wg.Done(); _, _ = s.Receive() }()
	<-started // reader goroutine is parked inside inner.Receive
	_ = s.Send(&v1.AttachRequest{})
	close(release)
	wg.Wait() // fakeStream reports any overlapping Receive via t.Error
	if st.recvCalls.Load() != 1 {
		t.Fatalf("recv calls = %d", st.recvCalls.Load())
	}
}

func TestAttachReceiveWaitsForInFlightDrain(t *testing.T) {
	t.Parallel()
	entered := make(chan struct{})
	release := make(chan struct{})
	st := &fakeStream{sendErr: io.EOF, recv: func(n int) (*v1.AttachEvent, error) {
		if n == 1 {
			close(entered)
			<-release
			return &v1.AttachEvent{}, nil
		}
		return nil, io.EOF
	}}
	_, s := attachRig(t, st)
	sent := make(chan struct{})
	go func() { _ = s.Send(&v1.AttachRequest{}); close(sent) }()
	<-entered // drain is in flight
	got := make(chan *v1.AttachEvent, 1)
	go func() { ev, _ := s.Receive(); got <- ev }()
	close(release) // fakeStream flags it if Receive overlapped the drain
	<-sent
	if ev := <-got; ev == nil {
		t.Fatal("Receive should return the drained event")
	}
}
