//go:build unix

package selecting

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

type fakeInfo struct {
	mode fs.FileMode
	uid  uint32
}

func (fakeInfo) Name() string        { return "" }
func (fakeInfo) Size() int64         { return 0 }
func (i fakeInfo) Mode() fs.FileMode { return i.mode }
func (fakeInfo) ModTime() time.Time  { return time.Time{} }
func (i fakeInfo) IsDir() bool       { return i.mode.IsDir() }
func (i fakeInfo) Sys() any          { return &syscall.Stat_t{Uid: i.uid} }

func fakeLstat(files map[string]fakeInfo) func(string) (fs.FileInfo, error) {
	return func(p string) (fs.FileInfo, error) {
		if fi, ok := files[p]; ok {
			return fi, nil
		}
		return nil, &fs.PathError{Op: "lstat", Path: p, Err: syscall.ENOENT}
	}
}

const (
	me, other = 1000, 2000
	sockPath  = "/tmp/tymuxd-2000/tymuxd.sock"
	dirPath   = "/tmp/tymuxd-2000"
)

func TestCheckSocketTrust(t *testing.T) {
	t.Parallel()
	sock := fs.ModeSocket | 0o660
	tests := []struct {
		name      string
		files     map[string]fakeInfo
		untrusted bool
	}{
		{"own socket in private dir", map[string]fakeInfo{sockPath: {sock, me}, dirPath: {fs.ModeDir | 0o700, me}}, false},
		{"foreign socket in world-writable sticky dir", map[string]fakeInfo{sockPath: {sock, other}, dirPath: {fs.ModeDir | fs.ModeSticky | 0o777, other}}, true},
		{"own socket in sticky world-writable dir", map[string]fakeInfo{sockPath: {sock, me}, dirPath: {fs.ModeDir | fs.ModeSticky | 0o777, 0}}, false},
		{"own socket in non-sticky world-writable dir", map[string]fakeInfo{sockPath: {sock, me}, dirPath: {fs.ModeDir | 0o777, me}}, true},
		{"foreign socket in private group dir (socket-group deployment)", map[string]fakeInfo{sockPath: {sock, other}, dirPath: {fs.ModeDir | 0o750, other}}, false},
		{"symlink", map[string]fakeInfo{sockPath: {fs.ModeSymlink | 0o777, me}, dirPath: {fs.ModeDir | 0o700, me}}, true},
		{"missing socket is left to the dial", map[string]fakeInfo{dirPath: {fs.ModeDir | 0o777, other}}, false},
		{"missing parent is left to the dial", map[string]fakeInfo{sockPath: {sock, other}}, false},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			err := checkSocketTrust(sockPath, me, fakeLstat(tc.files))
			if tc.untrusted != errors.Is(err, ErrSocketUntrusted) {
				t.Fatalf("err = %v, untrusted want %v", err, tc.untrusted)
			}
		})
	}
}

func TestRealSymlinkedSocketIsHardErrorNotFallback(t *testing.T) {
	t.Parallel()
	sock := shortSocketPath(t)
	serve(t, "unix", sock, "unix")
	link := sock + ".lnk"
	if err := os.Symlink(sock, link); err != nil {
		t.Fatal(err)
	}
	tcp, tcpH := serve(t, "tcp", "127.0.0.1:0", "tcp")
	c := New(Options{SocketPath: link, TCPBaseURL: "http://" + tcp.Addr().String(), Token: "tok", TokenOnTCP: true})
	defer c.CloseIdle()
	_, err := c.GetServerInfo(context.Background(), connect.NewRequest(&v1.GetServerInfoRequest{}))
	if !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("err = %v, want ErrSocketUntrusted", err)
	}
	if tcpH.lastAuth() != "" || c.Active() != Unix {
		t.Fatal("fell back to TCP on an untrusted socket")
	}
	_ = filepath.Base(link)
}
