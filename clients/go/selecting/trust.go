package selecting

import (
	"context"
	"crypto/tls"
	"errors"
	"io/fs"
	"net"
	"net/http"
	"os"
	"slices"
	"time"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	"github.com/tstapler/tymux/clients/go/authinterceptor"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// ErrSocketUntrusted matches (errors.Is) a refusal to use a control socket that another user could
// have planted: its path or owner failed the pre-dial check, or (Linux) the connected peer is not
// the caller, root or a trusted uid. It is a hard error: the client never falls back to TCP on it.
var ErrSocketUntrusted = errors.New("tymux: Unix control socket is not trusted")

// untrustedError carries the reason, matches ErrSocketUntrusted, and tells liveness.Probe the
// endpoint is a credentials/trust problem rather than a busy daemon.
type untrustedError struct{ msg string }

func (e *untrustedError) Error() string {
	if e.msg == "" {
		return ErrSocketUntrusted.Error()
	}
	return ErrSocketUntrusted.Error() + ": " + e.msg
}
func (e *untrustedError) Is(target error) bool { return target == ErrSocketUntrusted }
func (e *untrustedError) AccessDenied() bool   { return true }

// trustPolicy is who may own the socket: the caller, root, or an explicitly trusted uid.
type trustPolicy struct {
	uid     int
	trusted []uint32
}

func (p trustPolicy) trustedUID(owner int) bool {
	return owner >= 0 && slices.Contains(p.trusted, uint32(owner))
}

// allows: caller, root or opted-in.
func (p trustPolicy) allows(owner int) bool {
	return owner == p.uid || owner == 0 || p.trustedUID(owner)
}

// allowsNonRoot: caller or opted-in (root is not enough for a socket in a shared directory).
func (p trustPolicy) allowsNonRoot(owner int) bool { return owner == p.uid || p.trustedUID(owner) }

// trustFS is the filesystem seam for tests.
type trustFS struct {
	lstat, stat func(string) (fs.FileInfo, error)
	eval        func(string) (string, error)
}

func osFS() trustFS {
	return trustFS{lstat: os.Lstat, stat: os.Stat, eval: evalSymlinks}
}

// dialChecked vets socketPath, connects, and (Linux) checks the connected peer's uid, closing the
// TOCTOU window between the path check and the connect.
func dialChecked(ctx context.Context, socketPath string, pol trustPolicy) (net.Conn, error) {
	if err := checkSocketTrust(socketPath, pol, osFS()); err != nil {
		return nil, err
	}
	conn, err := (&net.Dialer{}).DialContext(ctx, "unix", socketPath)
	if err != nil {
		return nil, err
	}
	if err := verifyPeer(conn, pol); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return conn, nil
}

func newPolicy(trusted []uint32) trustPolicy {
	return trustPolicy{uid: os.Getuid(), trusted: trusted}
}

func dialUnixChecked(socketPath string, pol trustPolicy) *http.Client {
	return &http.Client{Transport: &http2.Transport{
		AllowHTTP: true,
		DialTLSContext: func(ctx context.Context, _, _ string, _ *tls.Config) (net.Conn, error) {
			return dialChecked(ctx, socketPath, pol)
		},
	}}
}

// CheckedDial returns a dialer for socketPath that applies the same trust checks as Client. Its
// signature matches liveness.Options.Dial: supervisors probing the socket must use it, and a
// Prober from NewUnixTransport, so the probe cannot hand a token to a planted socket.
// trustedUIDs is Options.TrustedSocketOwnerUIDs.
func CheckedDial(socketPath string, trustedUIDs []uint32) func(ctx context.Context, network, address string, timeout time.Duration) (net.Conn, error) {
	pol := newPolicy(trustedUIDs)
	return func(ctx context.Context, _, _ string, timeout time.Duration) (net.Conn, error) {
		if timeout > 0 {
			var cancel context.CancelFunc
			ctx, cancel = context.WithTimeout(ctx, timeout)
			defer cancel()
		}
		return dialChecked(ctx, socketPath, pol)
	}
}

// NewUnixTransport is a single-endpoint Transport over the trust-checked Unix socket, with no TCP
// fallback, for liveness.Probe and other callers that must not be masked by a fallback.
func NewUnixTransport(socketPath, token string, trustedUIDs []uint32) Transport {
	hc := dialUnixChecked(socketPath, newPolicy(trustedUIDs))
	return Wrap(tymuxv1connect.NewTymuxServiceClient(hc, "http://unix",
		connect.WithGRPC(), connect.WithInterceptors(authinterceptor.Interceptor{Token: token})))
}
