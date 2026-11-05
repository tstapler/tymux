//go:build unix

package selecting

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
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

// fakeTree models a filesystem: files is keyed by RESOLVED path, links maps a symlink path to its
// absolute target, and a link's own metadata is files[linkPath].
type fakeTree struct {
	files map[string]fakeInfo
	links map[string]string
}

func (t fakeTree) eval(p string) (string, error) {
	cur := "/"
	for _, e := range strings.Split(strings.Trim(filepath.Clean(p), "/"), "/") {
		if e == "" {
			continue
		}
		cur = filepath.Join(cur, e)
		if tgt, ok := t.links[cur]; ok {
			cur = tgt
		}
	}
	return cur, nil
}

func (t fakeTree) lstat(p string) (fs.FileInfo, error) {
	p = filepath.Clean(p)
	if _, isLink := t.links[p]; !isLink {
		rd, _ := t.eval(filepath.Dir(p))
		p = filepath.Join(rd, filepath.Base(p))
	}
	if fi, ok := t.files[p]; ok {
		return fi, nil
	}
	return nil, &fs.PathError{Op: "lstat", Path: p, Err: syscall.ENOENT}
}

func (t fakeTree) stat(p string) (fs.FileInfo, error) {
	rp, _ := t.eval(p)
	if fi, ok := t.files[rp]; ok {
		return fi, nil
	}
	return nil, &fs.PathError{Op: "stat", Path: p, Err: syscall.ENOENT}
}

func (t fakeTree) fsys() trustFS { return trustFS{lstat: t.lstat, stat: t.stat, eval: t.eval} }

const me, other, root = 501, 666, 0

var (
	sock = fs.ModeSocket | 0o660
	priv = fs.ModeDir | 0o700
	rlnk = fs.ModeSymlink | 0o755
)

func policy(trusted ...uint32) trustPolicy { return trustPolicy{uid: me, trusted: trusted} }

func TestCheckSocketTrust(t *testing.T) {
	t.Parallel()
	// macOS shape: /var and /tmp are root-owned symlinks into /private.
	macBase := func() fakeTree {
		return fakeTree{
			files: map[string]fakeInfo{
				"/var": {rlnk, root}, "/tmp": {rlnk, root}, "/private": {fs.ModeDir | 0o755, root},
				"/private/var": {fs.ModeDir | 0o755, root}, "/private/var/folders": {fs.ModeDir | 0o755, root},
				"/private/var/folders/xx": {fs.ModeDir | 0o755, root}, "/private/var/folders/xx/T": {fs.ModeDir | 0o700, me},
				"/private/tmp": {fs.ModeDir | fs.ModeSticky | 0o777, root},
			},
			links: map[string]string{"/var": "/private/var", "/tmp": "/private/tmp"},
		}
	}
	with := func(base fakeTree, files map[string]fakeInfo, links map[string]string) fakeTree {
		for k, v := range files {
			base.files[k] = v
		}
		for k, v := range links {
			base.links[k] = v
		}
		return base
	}
	const defaultMac = "/var/folders/xx/T/tymuxd-501/tymuxd.sock"
	macDefault := func() fakeTree {
		return with(macBase(), map[string]fakeInfo{
			"/private/var/folders/xx/T/tymuxd-501":             {priv, me},
			"/private/var/folders/xx/T/tymuxd-501/tymuxd.sock": {sock, me},
		}, nil)
	}
	tests := []struct {
		name      string
		path      string
		tree      fakeTree
		pol       trustPolicy
		untrusted bool
	}{
		{"macOS default TMPDIR shape with root-owned /var symlink", defaultMac, macDefault(), policy(), false},
		{"own dir and socket", "/run/user/501/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/run": {fs.ModeDir | 0o755, root}, "/run/user": {fs.ModeDir | 0o755, root}, "/run/user/501": {priv, me},
			"/run/user/501/tymuxd": {priv, me}, "/run/user/501/tymuxd/tymuxd.sock": {sock, me}}, links: nil}, policy(), false},
		{"root-owned dir and socket", "/run/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/run": {fs.ModeDir | 0o755, root}, "/run/tymuxd": {fs.ModeDir | 0o755, root}, "/run/tymuxd/tymuxd.sock": {sock, root}}}, policy(), false},
		{"attacker-owned squatted 0755 dir and socket", "/tmp/tymuxd-501/tymuxd.sock", with(macBase(), map[string]fakeInfo{
			"/private/tmp/tymuxd-501": {fs.ModeDir | 0o755, other}, "/private/tmp/tymuxd-501/tymuxd.sock": {sock, other}}, nil), policy(), true},
		{"other uid with explicit opt-in (group deployment)", "/srv/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/srv": {fs.ModeDir | 0o755, root}, "/srv/tymuxd": {fs.ModeDir | 0o750, 700}, "/srv/tymuxd/tymuxd.sock": {sock, 700}}}, policy(700), false},
		{"other uid without opt-in", "/srv/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/srv": {fs.ModeDir | 0o755, root}, "/srv/tymuxd": {fs.ModeDir | 0o750, 700}, "/srv/tymuxd/tymuxd.sock": {sock, 700}}}, policy(), true},
		{"dir owner differs from socket owner", "/srv/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/srv": {fs.ModeDir | 0o755, root}, "/srv/tymuxd": {priv, me}, "/srv/tymuxd/tymuxd.sock": {sock, root}}}, policy(), true},
		{"symlinked parent into attacker dir", "/home/u/link/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/home": {fs.ModeDir | 0o755, root}, "/home/u": {fs.ModeDir | 0o755, me}, "/home/u/link": {rlnk, other},
			"/evil": {fs.ModeDir | 0o755, other}, "/evil/tymuxd.sock": {sock, other}}, links: map[string]string{"/home/u/link": "/evil"}}, policy(), true},
		{"symlink owned by another user mid-path", "/home/u/link/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/home": {fs.ModeDir | 0o755, root}, "/home/u": {fs.ModeDir | 0o755, me}, "/home/u/link": {rlnk, other},
			"/home/u/real": {priv, me}, "/home/u/real/tymuxd.sock": {sock, me}}, links: map[string]string{"/home/u/link": "/home/u/real"}}, policy(), true},
		{"foreign socket directly under macOS-style /tmp symlink", "/tmp/x.sock", with(macBase(), map[string]fakeInfo{
			"/private/tmp/x.sock": {sock, other}}, nil), policy(), true},
		{"own socket directly in sticky /tmp", "/tmp/x.sock", with(macBase(), map[string]fakeInfo{
			"/private/tmp/x.sock": {sock, me}}, nil), policy(), false},
		{"root-owned socket in sticky /tmp refused for non-root", "/tmp/x.sock", with(macBase(), map[string]fakeInfo{
			"/private/tmp/x.sock": {sock, root}}, nil), policy(), true},
		{"non-sticky world-writable dir", "/shared/x.sock", fakeTree{files: map[string]fakeInfo{
			"/shared": {fs.ModeDir | 0o777, me}, "/shared/x.sock": {sock, me}}}, policy(), true},
		{"caller-owned symlink mid-path is fine", "/home/u/link/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/home": {fs.ModeDir | 0o755, root}, "/home/u": {fs.ModeDir | 0o755, me}, "/home/u/link": {rlnk, me},
			"/home/u/real": {priv, me}, "/home/u/real/tymuxd.sock": {sock, me}}, links: map[string]string{"/home/u/link": "/home/u/real"}}, policy(), false},
		{"caller-owned symlink resolving into an attacker dir", "/home/u/link/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/home": {fs.ModeDir | 0o755, root}, "/home/u": {fs.ModeDir | 0o755, me}, "/home/u/link": {rlnk, me},
			"/evil": {fs.ModeDir | 0o755, other}, "/evil/tymuxd.sock": {sock, other}}, links: map[string]string{"/home/u/link": "/evil"}}, policy(), true},
		{"relative path", "tymuxd.sock", macDefault(), policy(), true},
		{"dot-relative path", "./tymuxd.sock", macDefault(), policy(), true},
		{"empty path", "", macDefault(), policy(), true},
		{"dotdot after a symlink", "/home/u/lnk/../s", fakeTree{files: map[string]fakeInfo{
			"/home": {fs.ModeDir | 0o755, root}, "/home/u": {fs.ModeDir | 0o777, me}}}, policy(), true},
		{"double slash", "/run//tymuxd.sock", macDefault(), policy(), true},
		{"trailing slash", "/run/tymuxd.sock/", macDefault(), policy(), true},
		{"dot component", "/run/./tymuxd.sock", macDefault(), policy(), true},
		{"attacker-owned sticky world-writable dir holding our socket", "/tmp/tymuxd-501/tymuxd.sock", with(macBase(), map[string]fakeInfo{
			"/private/tmp/tymuxd-501": {fs.ModeDir | fs.ModeSticky | 0o777, other}, "/private/tmp/tymuxd-501/tymuxd.sock": {sock, me}}, nil), policy(), true},
		{"group-writable dir owned by another user", "/srv/d/s", fakeTree{files: map[string]fakeInfo{
			"/srv": {fs.ModeDir | 0o755, root}, "/srv/d": {fs.ModeDir | 0o770, other}, "/srv/d/s": {sock, other}}}, policy(other), true},
		{"group-writable dir owned by the caller", "/srv/d/s", fakeTree{files: map[string]fakeInfo{
			"/srv": {fs.ModeDir | 0o755, root}, "/srv/d": {fs.ModeDir | 0o770, me}, "/srv/d/s": {sock, me}}}, policy(), true},
		{"final element symlink", "/run/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/run": {fs.ModeDir | 0o755, root}, "/run/tymuxd": {priv, me}, "/run/tymuxd/tymuxd.sock": {rlnk, root}}}, policy(), true},
		{"missing socket is left to the dial", "/run/tymuxd/tymuxd.sock", fakeTree{files: map[string]fakeInfo{
			"/run": {fs.ModeDir | 0o755, root}, "/run/tymuxd": {priv, me}}}, policy(), false},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			err := checkSocketTrust(tc.path, tc.pol, tc.tree.fsys())
			if tc.untrusted != errors.Is(err, ErrSocketUntrusted) {
				t.Fatalf("err = %v, untrusted want %v", err, tc.untrusted)
			}
		})
	}
}

func TestRealSymlinkedSocketIsHardErrorNotFallback(t *testing.T) {
	t.Parallel()
	sockPath := shortSocketPath(t)
	serve(t, "unix", sockPath, "unix")
	link := sockPath + ".lnk"
	if err := os.Symlink(sockPath, link); err != nil {
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
}

// A symlink the caller owns is the caller's own choice, so a symlinked parent that resolves to a
// trusted directory still works (the hostile case is covered with a fake tree above).
func TestRealCallerOwnedSymlinkedParentStillWorks(t *testing.T) {
	t.Parallel()
	sockPath := shortSocketPath(t)
	serve(t, "unix", sockPath, "unix")
	link := filepath.Dir(sockPath) + ".dirlnk"
	if err := os.Symlink(filepath.Dir(sockPath), link); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Remove(link) })
	c := New(Options{SocketPath: filepath.Join(link, filepath.Base(sockPath))})
	defer c.CloseIdle()
	if _, err := c.GetServerInfo(context.Background(), connect.NewRequest(&v1.GetServerInfoRequest{})); err != nil {
		t.Fatal(err)
	}
}

func TestCheckedDialAndUnixTransportForSupervisors(t *testing.T) {
	t.Parallel()
	sockPath := shortSocketPath(t)
	serve(t, "unix", sockPath, "unix")
	conn, err := CheckedDial(sockPath, nil)(context.Background(), "unix", sockPath, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	_ = conn.Close()
	link := sockPath + ".lnk"
	if err := os.Symlink(sockPath, link); err != nil {
		t.Fatal(err)
	}
	if _, err := CheckedDial(link, nil)(context.Background(), "unix", link, time.Second); !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("symlink dial err = %v", err)
	}
	tr := NewUnixTransport(link, "", nil)
	if _, err := tr.GetServerInfo(context.Background(), connect.NewRequest(&v1.GetServerInfoRequest{})); !errors.Is(err, ErrSocketUntrusted) {
		t.Fatalf("transport err = %v", err)
	}
	var ad interface{ AccessDenied() bool }
	if !errors.As(&untrustedError{}, &ad) || !ad.AccessDenied() {
		t.Fatal("untrusted errors must report AccessDenied for liveness")
	}
}

func TestReprobeUntrustedSwitchesToUnixSoNextCallSurfacesIt(t *testing.T) {
	t.Parallel()
	r := newRig(Options{ReprobeInterval: time.Second})
	fallBackToTCP(t, r)
	r.unix.set(&untrustedError{msg: "planted"}, nil)
	r.clock.Advance(2 * time.Second)
	_ = list(t, r.c)
	if r.c.Active() != Unix {
		t.Fatal("stayed on tcp while the socket is untrusted")
	}
}
