package selecting

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"net"
	"net/http"
	"os"
	"path/filepath"

	"crypto/tls"

	"golang.org/x/net/http2"
)

// ErrSocketUntrusted matches (errors.Is) a refusal to dial a control socket that another user
// could have planted. It is a hard error: the client never falls back to TCP on it.
var ErrSocketUntrusted = errors.New("tymux: Unix control socket path is not trusted")

// checkSocketTrust vets the socket path before the token is sent to it. It refuses a symlink, and
// a socket in a world-writable directory unless that directory is sticky and the socket is ours
// (a non-sticky shared directory lets anyone replace it). A private parent directory is trusted
// whoever owns the socket, which keeps --socket-group deployments (daemon uid != client uid)
// working. Missing files are left to the dial so ENOENT can still select the TCP fallback.
func checkSocketTrust(path string, uid int, lstat func(string) (fs.FileInfo, error)) error {
	fi, err := lstat(path)
	if err != nil {
		return nil
	}
	if fi.Mode()&fs.ModeSymlink != 0 {
		return fmt.Errorf("%w: %s is a symlink", ErrSocketUntrusted, path)
	}
	dir := filepath.Dir(path)
	di, err := lstat(dir)
	if err != nil || di.Mode().Perm()&0o002 == 0 {
		return nil
	}
	if di.Mode()&fs.ModeSticky == 0 {
		return fmt.Errorf("%w: %s is world-writable and not sticky", ErrSocketUntrusted, dir)
	}
	if owner, ok := ownerUID(fi); ok && owner != uid {
		return fmt.Errorf("%w: %s is owned by uid %d in world-writable %s", ErrSocketUntrusted, path, owner, dir)
	}
	return nil
}

// dialUnixChecked is udsdialer.DialUnixHTTPClient with checkSocketTrust before every dial.
func dialUnixChecked(socketPath string) *http.Client {
	return &http.Client{Transport: &http2.Transport{
		AllowHTTP: true,
		DialTLSContext: func(ctx context.Context, _, _ string, _ *tls.Config) (net.Conn, error) {
			if err := checkSocketTrust(socketPath, os.Getuid(), os.Lstat); err != nil {
				return nil, err
			}
			return (&net.Dialer{}).DialContext(ctx, "unix", socketPath)
		},
	}}
}
