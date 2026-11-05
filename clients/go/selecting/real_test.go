package selecting

import (
	"context"
	"errors"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/h2c"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

type identHandler struct {
	tymuxv1connect.UnimplementedTymuxServiceHandler
	name string
	auth atomic.Value // last Authorization header seen by GetServerInfo
}

func (h *identHandler) GetServerInfo(_ context.Context, r *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error) {
	h.auth.Store(r.Header().Get("Authorization"))
	return connect.NewResponse(&v1.ServerInfo{Version: h.name}), nil
}

func (h *identHandler) lastAuth() string {
	v, _ := h.auth.Load().(string)
	return v
}

func (h *identHandler) Attach(_ context.Context, s *connect.BidiStream[v1.AttachRequest, v1.AttachEvent]) error {
	for {
		if _, err := s.Receive(); err != nil {
			return nil
		}
		if err := s.Send(&v1.AttachEvent{}); err != nil {
			return err
		}
	}
}

// serve runs an in-process h2c server and returns its listener; closing it simulates a daemon going away.
func serve(t *testing.T, network, addr, name string) (net.Listener, *identHandler) {
	t.Helper()
	l, err := net.Listen(network, addr)
	if err != nil {
		t.Fatal(err)
	}
	h := &identHandler{name: name}
	mux := http.NewServeMux()
	mux.Handle(tymuxv1connect.NewTymuxServiceHandler(h))
	srv := &http.Server{Handler: h2c.NewHandler(mux, &http2.Server{})}
	go func() { _ = srv.Serve(l) }()
	t.Cleanup(func() { _ = srv.Close() })
	return l, h
}

func shortSocketPath(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "sel")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	return filepath.Join(dir, "d.sock")
}

func serverName(t *testing.T, c *Client) string {
	t.Helper()
	resp, err := c.GetServerInfo(context.Background(), connect.NewRequest(&v1.GetServerInfoRequest{}))
	if err != nil {
		t.Fatalf("GetServerInfo: %v", err)
	}
	return resp.Msg.GetVersion()
}

func TestRealUnixPreferredOverTCP(t *testing.T) {
	t.Parallel()
	sock := shortSocketPath(t)
	serve(t, "unix", sock, "unix")
	tcp, _ := serve(t, "tcp", "127.0.0.1:0", "tcp")
	c := New(Options{SocketPath: sock, TCPBaseURL: "http://" + tcp.Addr().String()})
	defer c.CloseIdle()
	if got := serverName(t, c); got != "unix" {
		t.Fatalf("served by %q, want unix", got)
	}
}

func TestRealFallsBackWhenSocketAbsentOrStale(t *testing.T) {
	t.Parallel()
	tcp, _ := serve(t, "tcp", "127.0.0.1:0", "tcp")
	base := "http://" + tcp.Addr().String()

	t.Run("absent socket (ENOENT)", func(t *testing.T) {
		t.Parallel()
		c := New(Options{SocketPath: shortSocketPath(t), TCPBaseURL: base})
		defer c.CloseIdle()
		if got := serverName(t, c); got != "tcp" || c.Active() != TCP {
			t.Fatalf("served by %q active=%s", got, c.Active())
		}
	})

	t.Run("stale socket file (ECONNREFUSED)", func(t *testing.T) {
		t.Parallel()
		sock := shortSocketPath(t)
		l, err := net.Listen("unix", sock)
		if err != nil {
			t.Fatal(err)
		}
		ul := l.(*net.UnixListener)
		ul.SetUnlinkOnClose(false) // leave the file behind like a crashed daemon
		_ = ul.Close()
		c := New(Options{SocketPath: sock, TCPBaseURL: base})
		defer c.CloseIdle()
		if got := serverName(t, c); got != "tcp" {
			t.Fatalf("served by %q", got)
		}
	})
}

func TestRealEACCESNeverFallsBackToTCP(t *testing.T) {
	t.Parallel()
	if os.Geteuid() == 0 {
		t.Skip("root bypasses socket file permissions")
	}
	sock := shortSocketPath(t)
	serve(t, "unix", sock, "unix")
	tcp, _ := serve(t, "tcp", "127.0.0.1:0", "tcp")
	if err := os.Chmod(sock, 0); err != nil {
		t.Fatal(err)
	}
	c := New(Options{SocketPath: sock, TCPBaseURL: "http://" + tcp.Addr().String()})
	defer c.CloseIdle()
	_, err := c.GetServerInfo(context.Background(), connect.NewRequest(&v1.GetServerInfoRequest{}))
	if !errors.Is(err, ErrSocketAccessDenied) {
		t.Fatalf("err = %v, want ErrSocketAccessDenied", err)
	}
	if c.Active() != Unix {
		t.Fatal("selector left the unix transport on EACCES")
	}
}

func TestRealAttachRefusedDialThenReattachLandsOnTCP(t *testing.T) {
	t.Parallel()
	tcp, _ := serve(t, "tcp", "127.0.0.1:0", "tcp")
	c := New(Options{SocketPath: shortSocketPath(t), TCPBaseURL: "http://" + tcp.Addr().String()})
	defer c.CloseIdle()

	first := c.Attach(context.Background())
	err := first.Send(&v1.AttachRequest{})
	if !refusedForTest(err) {
		t.Fatalf("first Send = %v, want the real refused dial (not bare EOF)", err)
	}
	if c.Active() != TCP {
		t.Fatalf("active = %s after refused Attach", c.Active())
	}

	second := c.Attach(context.Background())
	if err := second.Send(&v1.AttachRequest{}); err != nil {
		t.Fatalf("second Send: %v", err)
	}
	if _, err := second.Receive(); err != nil {
		t.Fatalf("second Receive: %v", err)
	}
	_ = second.CloseRequest()
	_ = second.CloseResponse()
}

func TestRealTokenGoesToUnixOnlyUnlessTCPOptIn(t *testing.T) {
	t.Parallel()
	for _, optIn := range []bool{false, true} {
		sock := shortSocketPath(t) // absent: forces the TCP fallback
		tcp, tcpH := serve(t, "tcp", "127.0.0.1:0", "tcp")
		c := New(Options{SocketPath: sock, TCPBaseURL: "http://" + tcp.Addr().String(), Token: "tok", TokenOnTCP: optIn})
		if got := serverName(t, c); got != "tcp" {
			t.Fatalf("served by %q", got)
		}
		c.CloseIdle()
		want := ""
		if optIn {
			want = "Bearer tok"
		}
		if got := tcpH.lastAuth(); got != want {
			t.Fatalf("TokenOnTCP=%v: tcp saw Authorization %q, want %q", optIn, got, want)
		}
	}
}

func TestRealTokenSentOnUnix(t *testing.T) {
	t.Parallel()
	sock := shortSocketPath(t)
	_, uh := serve(t, "unix", sock, "unix")
	c := New(Options{SocketPath: sock, Token: "tok"})
	defer c.CloseIdle()
	_ = serverName(t, c)
	if got := uh.lastAuth(); got != "Bearer tok" {
		t.Fatalf("unix saw %q", got)
	}
}
