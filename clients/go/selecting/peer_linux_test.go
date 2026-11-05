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
