// Package capabilities feature-detects optional tymuxd behavior (ADR-0003).
//
// A daemon silently ignores request fields it does not know, so sending
// CreateSessionRequest.stderr_path, arg_specs, stdin_path or stdin_stream to an older daemon succeeds
// and does nothing. Gate each use on Has.
package capabilities

import (
	"context"

	"connectrpc.com/connect"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// Capability strings advertised in ServerInfo.capabilities.
const (
	ArgsEnv        = "create_session.args_env"        // CreateSessionRequest.args, env
	StderrRedirect = "create_session.stderr_redirect" // CreateSessionRequest.stderr_path
	FileArgs       = "create_session.file_args"       // CreateSessionRequest.arg_specs
	StdinFile      = "create_session.stdin_file"      // CreateSessionRequest.stdin_path
	StdinStream    = "create_session.stdin_stream"    // CreateSessionRequest.stdin_stream, WriteStdin RPC
	PaneLifecycle  = "pane.lifecycle"                 // restart_policy, GetPaneStatus, RestartPane, StopPane, StartPane, ResetRestartBudget, AttachRequest.subscribe_lifecycle
)

// Set is the capabilities a daemon advertises.
type Set map[string]struct{}

// Has reports whether the daemon advertised name.
func (s Set) Has(name string) bool {
	_, ok := s[name]
	return ok
}

// Detect asks the daemon what it supports. A daemon older than GetServerInfo
// (<= 1.3.0) answers UNIMPLEMENTED: that is the empty set, not an error.
func Detect(ctx context.Context, client tymuxv1connect.TymuxServiceClient) (Set, error) {
	resp, err := client.GetServerInfo(ctx, connect.NewRequest(&tymuxv1.GetServerInfoRequest{}))
	if err != nil {
		if connect.CodeOf(err) == connect.CodeUnimplemented {
			return Set{}, nil
		}
		return nil, err
	}
	set := make(Set, len(resp.Msg.GetCapabilities()))
	for _, c := range resp.Msg.GetCapabilities() {
		set[c] = struct{}{}
	}
	return set, nil
}
