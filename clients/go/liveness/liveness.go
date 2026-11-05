// Package liveness classifies whether a tymuxd endpoint is usable, for supervisors that
// must not kill or respawn a daemon on weak evidence.
package liveness

import (
	"context"
	"errors"
	"net"
	"syscall"
	"time"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// Result orders probe outcomes from worst to best.
type Result int

const (
	// NotListening: a definite refusal (ECONNREFUSED, or ENOENT for a unix socket).
	NotListening Result = iota
	// Unhealthy: something answered but not as tymuxd, or the caller's ctx ended (no evidence either way,
	// never usable, never proof the daemon is down).
	Unhealthy
	// Busy: the dial or GetServerInfo timed out, or the dial failed with something other than a
	// refusal (fd exhaustion, unreachable...). Inconclusive, so treated as usable.
	Busy
	// Healthy: GetServerInfo answered.
	Healthy
)

func (r Result) String() string {
	return [...]string{"not-listening", "unhealthy", "busy", "healthy"}[r]
}

// Usable reports whether callers should keep using, and never kill, the daemon.
func (r Result) Usable() bool { return r >= Busy }

// Prober is the one RPC Probe needs. A generated connect client bound to the single endpoint
// satisfies it; do not pass a *selecting.Client, whose failover would let TCP mask a dead socket.
type Prober interface {
	GetServerInfo(context.Context, *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error)
}

// Logger is satisfied by *slog.Logger.
type Logger interface {
	Warn(msg string, args ...any)
}

// Options tunes Probe. The zero value is usable.
type Options struct {
	DialTimeout  time.Duration // default 5s
	ProbeTimeout time.Duration // default 10s, for GetServerInfo: a loaded daemon answers slowly
	// Dial is the raw-dial seam (default net.Dialer.DialContext with DialTimeout).
	Dial   func(ctx context.Context, network, address string, timeout time.Duration) (net.Conn, error)
	Logger Logger
}

// Probe dials network/address, then calls GetServerInfo (O(1), unlike ListSessions) on p.
// A busy daemon is not a dead one: only a definite refusal reports NotListening.
// Probe checks one endpoint; to check Unix-then-TCP, call it per endpoint and never consult TCP
// when Unix is Busy. Pass a p that targets exactly this endpoint, so a healthy fallback cannot mask it.
func Probe(ctx context.Context, network, address string, p Prober, opts Options) Result {
	if opts.DialTimeout <= 0 {
		opts.DialTimeout = 5 * time.Second
	}
	if opts.ProbeTimeout <= 0 {
		opts.ProbeTimeout = 10 * time.Second
	}
	dial := opts.Dial
	if dial == nil {
		dial = func(ctx context.Context, network, address string, timeout time.Duration) (net.Conn, error) {
			return (&net.Dialer{Timeout: timeout}).DialContext(ctx, network, address)
		}
	}
	warn := func(msg string, args ...any) {
		if opts.Logger != nil {
			opts.Logger.Warn(msg, append(args, "network", network, "address", address)...)
		}
	}

	conn, err := dial(ctx, network, address, opts.DialTimeout)
	if err != nil {
		switch {
		case ctx.Err() != nil:
			return Unhealthy
		case dialRefused(network, err):
			warn("tymuxd probe: nothing listening", "err", err)
			return NotListening
		default:
			warn("tymuxd probe: dial inconclusive, treating as busy", "err", err)
			return Busy
		}
	}
	_ = conn.Close()

	healthCtx, cancel := context.WithTimeout(ctx, opts.ProbeTimeout)
	defer cancel()
	_, err = p.GetServerInfo(healthCtx, connect.NewRequest(&v1.GetServerInfoRequest{}))
	switch {
	case err == nil:
		return Healthy
	case ctx.Err() != nil:
		return Unhealthy
	case errors.Is(healthCtx.Err(), context.DeadlineExceeded) || connect.CodeOf(err) == connect.CodeDeadlineExceeded:
		warn("tymuxd probe: no answer before the timeout, treating as busy")
		return Busy
	default:
		warn("tymuxd probe: listener did not answer as tymuxd", "err", err)
		return Unhealthy
	}
}

// dialRefused: only ECONNREFUSED (and ENOENT for unix paths) prove nothing listens. Timeouts and
// resource errors say nothing, and mistaking them for "down" under fd exhaustion would get a live
// daemon holding every pane killed.
func dialRefused(network string, err error) bool {
	return errors.Is(err, syscall.ECONNREFUSED) || (network == "unix" && errors.Is(err, syscall.ENOENT))
}
