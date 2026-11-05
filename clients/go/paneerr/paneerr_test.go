package paneerr

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"testing"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/h2c"
	"google.golang.org/protobuf/encoding/protowire"
	"google.golang.org/protobuf/types/known/anypb"

	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// errorInfoBytes encodes google.rpc.ErrorInfo{reason, domain} (plus a metadata entry, which Reason must skip).
func errorInfoBytes(reason, domain string) []byte {
	var b []byte
	b = protowire.AppendTag(b, 1, protowire.BytesType)
	b = protowire.AppendString(b, reason)
	b = protowire.AppendTag(b, 2, protowire.BytesType)
	b = protowire.AppendString(b, domain)
	b = protowire.AppendTag(b, 3, protowire.BytesType) // map<string,string> metadata entry
	b = protowire.AppendBytes(b, append(
		protowire.AppendString(protowire.AppendTag(nil, 1, protowire.BytesType), "pane_id"),
		protowire.AppendString(protowire.AppendTag(nil, 2, protowire.BytesType), "p1")...))
	return b
}

func failedPrecondition(t *testing.T, code connect.Code, reason, domain string) error {
	t.Helper()
	err := connect.NewError(code, errors.New("pane is not running"))
	d, derr := connect.NewErrorDetail(&anypb.Any{
		TypeUrl: "type.googleapis.com/google.rpc.ErrorInfo",
		Value:   errorInfoBytes(reason, domain),
	})
	if derr != nil {
		t.Fatal(derr)
	}
	err.AddDetail(d)
	return err
}

func TestClassification(t *testing.T) {
	t.Parallel()
	exited := failedPrecondition(t, connect.CodeFailedPrecondition, ReasonPaneExited, Domain)
	restored := failedPrecondition(t, connect.CodeFailedPrecondition, ReasonPaneRestoredNoScreen, Domain)
	tests := []struct {
		name                         string
		err                          error
		exited, restored, notRunning bool
	}{
		{"exited", exited, true, false, true},
		{"restored", restored, false, true, true},
		{"wrapped", fmt.Errorf("attach: %w", exited), true, false, true},
		{"foreign domain", failedPrecondition(t, connect.CodeFailedPrecondition, ReasonPaneExited, "other.v1"), false, false, false},
		{"wrong code", failedPrecondition(t, connect.CodeInternal, ReasonPaneExited, Domain), false, false, false},
		{"unknown reason", failedPrecondition(t, connect.CodeFailedPrecondition, "SOMETHING_ELSE", Domain), false, false, false},
		{"plain failed precondition", connect.NewError(connect.CodeFailedPrecondition, errors.New("no detail")), false, false, false},
		{"non-connect error", errors.New("boom"), false, false, false},
		{"nil", nil, false, false, false},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			if IsPaneExited(tc.err) != tc.exited || IsPaneRestoredWithoutScreen(tc.err) != tc.restored || IsPaneNotRunning(tc.err) != tc.notRunning {
				t.Fatalf("exited=%v restored=%v notRunning=%v", IsPaneExited(tc.err), IsPaneRestoredWithoutScreen(tc.err), IsPaneNotRunning(tc.err))
			}
		})
	}
}

func TestMalformedDetailIsIgnored(t *testing.T) {
	t.Parallel()
	err := connect.NewError(connect.CodeFailedPrecondition, errors.New("x"))
	d, _ := connect.NewErrorDetail(&anypb.Any{TypeUrl: "type.googleapis.com/google.rpc.ErrorInfo", Value: []byte{0xff, 0xff, 0xff}})
	err.AddDetail(d)
	if IsPaneNotRunning(err) {
		t.Fatal("malformed ErrorInfo classified as pane-not-running")
	}
}

type deadPaneHandler struct {
	tymuxv1connect.UnimplementedTymuxServiceHandler
}

func (deadPaneHandler) CapturePane(context.Context, *connect.Request[v1.CapturePaneRequest]) (*connect.Response[v1.PaneSnapshot], error) {
	err := connect.NewError(connect.CodeFailedPrecondition, errors.New("pane was restored without a screen"))
	d, _ := connect.NewErrorDetail(&anypb.Any{
		TypeUrl: "type.googleapis.com/google.rpc.ErrorInfo",
		Value:   errorInfoBytes(ReasonPaneRestoredNoScreen, Domain),
	})
	err.AddDetail(d)
	return nil, err
}

// The detail must survive the gRPC wire (grpc-status-details-bin), not just in-process construction.
func TestReasonSurvivesGRPCWire(t *testing.T) {
	t.Parallel()
	mux := http.NewServeMux()
	mux.Handle(tymuxv1connect.NewTymuxServiceHandler(deadPaneHandler{}))
	srv := httptest.NewServer(h2c.NewHandler(mux, &http2.Server{}))
	t.Cleanup(srv.Close)
	hc := &http.Client{Transport: &http2.Transport{AllowHTTP: true, DialTLSContext: dialPlain}}
	c := tymuxv1connect.NewTymuxServiceClient(hc, srv.URL, connect.WithGRPC())
	_, err := c.CapturePane(context.Background(), connect.NewRequest(&v1.CapturePaneRequest{}))
	if !IsPaneRestoredWithoutScreen(err) {
		t.Fatalf("Reason(%v) = %q", err, Reason(err))
	}
}

func dialPlain(ctx context.Context, network, addr string, _ *tls.Config) (net.Conn, error) {
	return (&net.Dialer{}).DialContext(ctx, network, addr)
}
