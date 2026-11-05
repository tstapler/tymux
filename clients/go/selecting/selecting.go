// Package selecting provides a tymuxd client that prefers the Unix control socket and
// falls back to the deprecated TCP loopback listener only when the socket is absent or
// refusing connections.
//
// Security: EACCES on the socket means a daemon is listening and denied this user. That
// is a hard error (ErrSocketAccessDenied) and never falls back to the unauthenticated
// TCP listener (project_plans/unix-socket-auth, loopback-trust ADR).
package selecting

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"sync"
	"syscall"
	"time"

	"connectrpc.com/connect"

	"github.com/tstapler/tymux/clients/go/authinterceptor"
	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// Kind names a transport.
type Kind string

const (
	Unix Kind = "unix"
	TCP  Kind = "tcp"
)

// DefaultTCPBaseURL is tymuxd's documented loopback listen address.
const DefaultTCPBaseURL = "http://127.0.0.1:7419"

const (
	defaultProbeTimeout    = 3 * time.Second
	defaultReprobeInterval = 30 * time.Second
)

// ErrSocketAccessDenied matches (errors.Is) an EACCES from the control socket.
var ErrSocketAccessDenied = errors.New("tymux: not authorized to use tymuxd's Unix control socket")

// Logger is satisfied by *slog.Logger.
type Logger interface {
	Debug(msg string, args ...any)
	Warn(msg string, args ...any)
}

// Options configures New and NewFromTransports. The zero value is usable.
type Options struct {
	// SocketPath defaults to udsdialer.ResolveSocketPath(os.Getuid()). New only.
	SocketPath string
	// TCPBaseURL defaults to DefaultTCPBaseURL. New only.
	TCPBaseURL string
	// Token is sent as a bearer token on both transports. New only.
	Token string
	// ClientOptions are appended to the connect clients New builds (e.g. tracing interceptors).
	ClientOptions []connect.ClientOption

	// ProbeTimeout bounds each GetServerInfo probe (default 3s), in addition to the caller's ctx.
	ProbeTimeout time.Duration
	// ReprobeInterval is how long a TCP fallback sticks before the next call re-probes
	// the Unix socket (default 30s). The re-probe runs on a caller's RPC, never a timer.
	ReprobeInterval time.Duration
	// Quiet is for one-shot probes: OnTransition is not called and Logger gets Debug only.
	Quiet bool
	// OnTransition is called after the active transport changes (not in Quiet mode).
	OnTransition func(from, to Kind, why string)
	Logger       Logger
	// Now is the clock for re-probe scheduling (default time.Now).
	Now func() time.Time
}

// Client is a Transport that starts on Unix and fails over to TCP. Safe for concurrent use.
//
// Failover happens only on a refused dial (ECONNREFUSED/ENOENT) and the call is retried
// at most once, so a half-sent RPC is never replayed. Existing Attach streams are
// untouched by a switch; only new calls and new Attach dials follow the choice.
type Client struct {
	unix, tcp Transport
	closers   []*http.Client
	opts      Options

	mu            sync.Mutex
	useTCP        bool
	nextUnixProbe time.Time
}

var _ Transport = (*Client)(nil)

// New builds a Client over real h2c connections to the Unix socket and TCP loopback.
func New(opts Options) *Client {
	if opts.SocketPath == "" {
		opts.SocketPath = udsdialer.ResolveSocketPath(os.Getuid())
	}
	if opts.TCPBaseURL == "" {
		opts.TCPBaseURL = DefaultTCPBaseURL
	}
	copts := append([]connect.ClientOption{
		connect.WithGRPC(),
		connect.WithInterceptors(authinterceptor.Interceptor{Token: opts.Token}),
	}, opts.ClientOptions...)
	unixHTTP, tcpHTTP := udsdialer.DialUnixHTTPClient(opts.SocketPath), udsdialer.DialTCPHTTPClient()
	c := NewFromTransports(
		Wrap(tymuxv1connect.NewTymuxServiceClient(unixHTTP, "http://unix", copts...)),
		Wrap(tymuxv1connect.NewTymuxServiceClient(tcpHTTP, opts.TCPBaseURL, copts...)),
		opts)
	c.closers = []*http.Client{unixHTTP, tcpHTTP}
	return c
}

// NewFromTransports selects between caller-supplied transports. Either may be nil: a
// nil tcp disables fallback, a nil unix pins TCP.
func NewFromTransports(unix, tcp Transport, opts Options) *Client {
	if unix == nil && tcp == nil {
		panic("selecting: NewFromTransports needs at least one transport")
	}
	if opts.ProbeTimeout <= 0 {
		opts.ProbeTimeout = defaultProbeTimeout
	}
	if opts.ReprobeInterval <= 0 {
		opts.ReprobeInterval = defaultReprobeInterval
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	return &Client{unix: unix, tcp: tcp, opts: opts, useTCP: unix == nil}
}

// Active reports the transport new calls use.
func (c *Client) Active() Kind {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.activeLocked()
}

func (c *Client) activeLocked() Kind {
	if c.useTCP {
		return TCP
	}
	return Unix
}

// CloseIdle drops pooled connections so a one-shot caller leaves nothing open on tymuxd.
// Defer it after a Quiet probe. It does not affect in-flight streams.
func (c *Client) CloseIdle() {
	for _, h := range c.closers {
		h.CloseIdleConnections()
	}
}

// pick returns the transport for a new call, first re-probing the Unix socket when a TCP
// fallback is due. Exactly one caller claims each due probe.
func (c *Client) pick(ctx context.Context) (Transport, Kind) {
	c.mu.Lock()
	if c.useTCP && c.unix != nil && !c.opts.Now().Before(c.nextUnixProbe) {
		prev := c.nextUnixProbe
		c.nextUnixProbe = c.opts.Now().Add(c.opts.ReprobeInterval)
		c.mu.Unlock()
		err := c.probe(ctx, c.unix)
		switch {
		case err == nil:
			c.switchTo(Unix, "control socket answers again")
		case deniedAccess(err):
			// A socket now exists and denies us: surface that instead of staying on TCP.
			c.switchTo(Unix, "control socket denies access")
		case ctx.Err() != nil:
			c.mu.Lock() // our own cancellation says nothing about the socket: return the claim
			c.nextUnixProbe = prev
			c.mu.Unlock()
		}
		c.mu.Lock()
	}
	defer c.mu.Unlock()
	if c.useTCP {
		return c.tcp, TCP
	}
	return c.unix, Unix
}

// probe is bound by the caller's ctx as well as ProbeTimeout, so a cancelled RPC never waits on it.
func (c *Client) probe(ctx context.Context, t Transport) error {
	ctx, cancel := context.WithTimeout(ctx, c.opts.ProbeTimeout)
	defer cancel()
	_, err := t.GetServerInfo(ctx, connect.NewRequest(&v1.GetServerInfoRequest{}))
	return err
}

func (c *Client) switchTo(to Kind, why string) {
	c.mu.Lock()
	from := c.activeLocked()
	if from == to {
		c.mu.Unlock()
		return
	}
	c.useTCP = to == TCP
	if c.useTCP {
		c.nextUnixProbe = c.opts.Now().Add(c.opts.ReprobeInterval)
	}
	c.mu.Unlock()
	if c.opts.Quiet {
		c.log(false, "tymux probe transport changed", from, to, why)
		return
	}
	c.log(true, "tymuxd transport changed", from, to, why)
	if c.opts.OnTransition != nil {
		c.opts.OnTransition(from, to, why)
	}
}

func (c *Client) log(warn bool, msg string, from, to Kind, why string) {
	switch l := c.opts.Logger; {
	case l == nil:
	case warn:
		l.Warn(msg, "from", string(from), "to", string(to), "why", why)
	default:
		l.Debug(msg, "from", string(from), "to", string(to), "why", why)
	}
}

// IsDialRefused reports whether err proves the endpoint has no listener.
func IsDialRefused(err error) bool {
	return errors.Is(err, syscall.ECONNREFUSED) || errors.Is(err, syscall.ENOENT)
}

// deniedAccess is a kernel-level (EACCES/EPERM) or daemon-level (PermissionDenied/Unauthenticated)
// refusal from a daemon that is listening: never a reason to use TCP.
func deniedAccess(err error) bool {
	switch connect.CodeOf(err) {
	case connect.CodePermissionDenied, connect.CodeUnauthenticated:
		return true
	}
	return errors.Is(err, syscall.EACCES) || errors.Is(err, syscall.EPERM)
}

// annotate makes an EACCES from the control socket a clear, matchable hard error.
func annotate(kind Kind, err error) error {
	if kind == Unix && (errors.Is(err, syscall.EACCES) || errors.Is(err, syscall.EPERM)) && !errors.Is(err, ErrSocketAccessDenied) {
		return fmt.Errorf("%w (ask the daemon's owner to add you to its --socket-group, or run as its OS user): %w",
			ErrSocketAccessDenied, err)
	}
	return err
}

// failover reacts to a failed call over from. It reports whether the active transport is
// now different from from (or already was), so the caller may retry once. Only a refused
// dial qualifies: EACCES and every other error are returned as they are.
func (c *Client) failover(ctx context.Context, from Kind, err error) bool {
	if err == nil || !IsDialRefused(err) {
		return false
	}
	if c.Active() != from {
		return true // another caller already switched
	}
	switch from {
	case Unix:
		if c.tcp == nil || c.probe(ctx, c.tcp) != nil {
			return false
		}
		c.switchTo(TCP, "control socket refused connections while the tcp listener answers")
	default:
		if c.unix == nil {
			return false
		}
		c.switchTo(Unix, "tcp listener refused connections")
	}
	return true
}

func invoke[Resp any](ctx context.Context, c *Client, call func(Transport) (*connect.Response[Resp], error)) (*connect.Response[Resp], error) {
	t, kind := c.pick(ctx)
	resp, err := call(t)
	if err != nil && c.failover(ctx, kind, err) {
		t, kind = c.pick(ctx)
		resp, err = call(t)
	}
	return resp, annotate(kind, err)
}

func (c *Client) GetServerInfo(ctx context.Context, r *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.ServerInfo], error) { return t.GetServerInfo(ctx, r) })
}

func (c *Client) CreateSession(ctx context.Context, r *connect.Request[v1.CreateSessionRequest]) (*connect.Response[v1.Session], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.Session], error) { return t.CreateSession(ctx, r) })
}

func (c *Client) ListSessions(ctx context.Context, r *connect.Request[v1.ListSessionsRequest]) (*connect.Response[v1.ListSessionsResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.ListSessionsResponse], error) { return t.ListSessions(ctx, r) })
}

func (c *Client) KillSession(ctx context.Context, r *connect.Request[v1.KillSessionRequest]) (*connect.Response[v1.KillSessionResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.KillSessionResponse], error) { return t.KillSession(ctx, r) })
}

func (c *Client) ReviveSession(ctx context.Context, r *connect.Request[v1.ReviveSessionRequest]) (*connect.Response[v1.ReviveSessionResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.ReviveSessionResponse], error) { return t.ReviveSession(ctx, r) })
}

func (c *Client) CapturePane(ctx context.Context, r *connect.Request[v1.CapturePaneRequest]) (*connect.Response[v1.PaneSnapshot], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneSnapshot], error) { return t.CapturePane(ctx, r) })
}

func (c *Client) SearchScrollback(ctx context.Context, r *connect.Request[v1.SearchScrollbackRequest]) (*connect.Response[v1.SearchScrollbackResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.SearchScrollbackResponse], error) {
		return t.SearchScrollback(ctx, r)
	})
}

func (c *Client) SplitPane(ctx context.Context, r *connect.Request[v1.SplitPaneRequest]) (*connect.Response[v1.Session], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.Session], error) { return t.SplitPane(ctx, r) })
}

func (c *Client) ClosePane(ctx context.Context, r *connect.Request[v1.ClosePaneRequest]) (*connect.Response[v1.ClosePaneResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.ClosePaneResponse], error) { return t.ClosePane(ctx, r) })
}

func (c *Client) CreateWindow(ctx context.Context, r *connect.Request[v1.CreateWindowRequest]) (*connect.Response[v1.Session], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.Session], error) { return t.CreateWindow(ctx, r) })
}

func (c *Client) SetRestartPolicy(ctx context.Context, r *connect.Request[v1.SetRestartPolicyRequest]) (*connect.Response[v1.SetRestartPolicyResponse], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.SetRestartPolicyResponse], error) {
		return t.SetRestartPolicy(ctx, r)
	})
}

func (c *Client) GetPaneStatus(ctx context.Context, r *connect.Request[v1.GetPaneStatusRequest]) (*connect.Response[v1.PaneStatus], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneStatus], error) { return t.GetPaneStatus(ctx, r) })
}

func (c *Client) RestartPane(ctx context.Context, r *connect.Request[v1.RestartPaneRequest]) (*connect.Response[v1.PaneStatus], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneStatus], error) { return t.RestartPane(ctx, r) })
}

func (c *Client) StopPane(ctx context.Context, r *connect.Request[v1.StopPaneRequest]) (*connect.Response[v1.PaneStatus], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneStatus], error) { return t.StopPane(ctx, r) })
}

func (c *Client) StartPane(ctx context.Context, r *connect.Request[v1.StartPaneRequest]) (*connect.Response[v1.PaneStatus], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneStatus], error) { return t.StartPane(ctx, r) })
}

func (c *Client) ResetRestartBudget(ctx context.Context, r *connect.Request[v1.ResetRestartBudgetRequest]) (*connect.Response[v1.PaneStatus], error) {
	return invoke(ctx, c, func(t Transport) (*connect.Response[v1.PaneStatus], error) { return t.ResetRestartBudget(ctx, r) })
}
