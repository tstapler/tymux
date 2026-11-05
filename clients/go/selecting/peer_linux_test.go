//go:build linux

package selecting

import (
	"errors"
	"net"
	"os"
	"testing"
)

func TestVerifyPeerChecksSocketPeerUID(t *testing.T) {
	t.Parallel()
	if os.Getuid() == 0 {
		t.Skip("root peers are always allowed")
	}
	sock := shortSocketPath(t)
	l, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			_ = c.Close()
		}
	}()
	conn, err := net.Dial("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	me := os.Getuid()
	if err := verifyPeer(conn, trustPolicy{uid: me}); err != nil {
		t.Fatalf("own peer rejected: %v", err)
	}
	if err := verifyPeer(conn, trustPolicy{uid: me + 1}); !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("foreign peer accepted: %v", err)
	}
	if err := verifyPeer(conn, trustPolicy{uid: me + 1, trusted: []uint32{uint32(me)}}); err != nil {
		t.Fatalf("opted-in peer rejected: %v", err)
	}
}

func TestVerifyPeerFailsClosed(t *testing.T) {
	t.Parallel()
	a, b := net.Pipe()
	defer a.Close()
	defer b.Close()
	if err := verifyPeer(a, trustPolicy{uid: os.Getuid()}); !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("non-unix conn accepted: %v", err)
	}
	sock := shortSocketPath(t)
	l, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	c, err := net.Dial("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	_ = c.Close() // Control on a closed conn errors: credentials unavailable
	if err := verifyPeer(c, trustPolicy{uid: os.Getuid()}); !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("unreadable credentials accepted: %v", err)
	}
}
