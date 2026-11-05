package selecting

import (
	"context"

	"connectrpc.com/connect"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// Transport is the RPC surface a Client selects over: every unary RPC plus Attach.
// WatchWindow and WriteStdin are not included; use a plain connect client for them.
type Transport interface {
	GetServerInfo(context.Context, *connect.Request[v1.GetServerInfoRequest]) (*connect.Response[v1.ServerInfo], error)
	CreateSession(context.Context, *connect.Request[v1.CreateSessionRequest]) (*connect.Response[v1.Session], error)
	ListSessions(context.Context, *connect.Request[v1.ListSessionsRequest]) (*connect.Response[v1.ListSessionsResponse], error)
	KillSession(context.Context, *connect.Request[v1.KillSessionRequest]) (*connect.Response[v1.KillSessionResponse], error)
	ReviveSession(context.Context, *connect.Request[v1.ReviveSessionRequest]) (*connect.Response[v1.ReviveSessionResponse], error)
	CapturePane(context.Context, *connect.Request[v1.CapturePaneRequest]) (*connect.Response[v1.PaneSnapshot], error)
	SearchScrollback(context.Context, *connect.Request[v1.SearchScrollbackRequest]) (*connect.Response[v1.SearchScrollbackResponse], error)
	SplitPane(context.Context, *connect.Request[v1.SplitPaneRequest]) (*connect.Response[v1.Session], error)
	ClosePane(context.Context, *connect.Request[v1.ClosePaneRequest]) (*connect.Response[v1.ClosePaneResponse], error)
	CreateWindow(context.Context, *connect.Request[v1.CreateWindowRequest]) (*connect.Response[v1.Session], error)
	SetRestartPolicy(context.Context, *connect.Request[v1.SetRestartPolicyRequest]) (*connect.Response[v1.SetRestartPolicyResponse], error)
	GetPaneStatus(context.Context, *connect.Request[v1.GetPaneStatusRequest]) (*connect.Response[v1.PaneStatus], error)
	RestartPane(context.Context, *connect.Request[v1.RestartPaneRequest]) (*connect.Response[v1.PaneStatus], error)
	StopPane(context.Context, *connect.Request[v1.StopPaneRequest]) (*connect.Response[v1.PaneStatus], error)
	StartPane(context.Context, *connect.Request[v1.StartPaneRequest]) (*connect.Response[v1.PaneStatus], error)
	ResetRestartBudget(context.Context, *connect.Request[v1.ResetRestartBudgetRequest]) (*connect.Response[v1.PaneStatus], error)
	// Attach opens the bidi stream. The dial is lazy, so a refused dial surfaces on
	// the first Send/Receive, not here.
	Attach(context.Context) AttachStream
}

// AttachStream is the subset of *connect.BidiStreamForClient[v1.AttachRequest, v1.AttachEvent]
// that Attach callers use.
type AttachStream interface {
	Send(*v1.AttachRequest) error
	Receive() (*v1.AttachEvent, error)
	CloseRequest() error
	CloseResponse() error
}

var _ AttachStream = (*connect.BidiStreamForClient[v1.AttachRequest, v1.AttachEvent])(nil)

// Wrap adapts a generated connect client to Transport.
func Wrap(c tymuxv1connect.TymuxServiceClient) Transport { return connectTransport{c} }

type connectTransport struct {
	tymuxv1connect.TymuxServiceClient
}

func (t connectTransport) Attach(ctx context.Context) AttachStream {
	return t.TymuxServiceClient.Attach(ctx)
}
