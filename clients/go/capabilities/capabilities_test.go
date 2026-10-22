package capabilities_test

import (
	"bytes"
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"

	"connectrpc.com/connect"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/reflect/protoregistry"
	"google.golang.org/protobuf/types/descriptorpb"
	"google.golang.org/protobuf/types/dynamicpb"

	"github.com/tstapler/tymux/clients/go/capabilities"
	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// oldDaemon behaves like tymuxd <= 1.3.0: no GetServerInfo, and a
// CreateSessionRequest schema that stops at field 5 (env). Requests are decoded
// with that schema and re-encoded without the unknown fields, as an old
// daemon's generated code does.
type oldDaemon struct {
	tymuxv1connect.UnimplementedTymuxServiceHandler
	seen *tymuxv1.CreateSessionRequest
}

func (d *oldDaemon) CreateSession(_ context.Context, req *connect.Request[tymuxv1.CreateSessionRequest]) (*connect.Response[tymuxv1.Session], error) {
	d.seen = req.Msg
	return connect.NewResponse(&tymuxv1.Session{Id: "old-session"}), nil
}

func oldCreateSessionDescriptor(t *testing.T) protoreflect.MessageDescriptor {
	t.Helper()
	fdp := protodesc.ToFileDescriptorProto(tymuxv1.File_tymux_v1_tymux_proto)
	for _, m := range fdp.MessageType {
		if m.GetName() != "CreateSessionRequest" {
			continue
		}
		var kept []*descriptorpb.FieldDescriptorProto
		for _, f := range m.Field {
			if f.GetNumber() <= 5 {
				kept = append(kept, f)
			}
		}
		m.Field = kept
	}
	fd, err := protodesc.NewFile(fdp, protoregistry.GlobalFiles)
	if err != nil {
		t.Fatalf("build old schema: %v", err)
	}
	return fd.Messages().ByName("CreateSessionRequest")
}

func startOldDaemon(t *testing.T) (tymuxv1connect.TymuxServiceClient, *oldDaemon) {
	t.Helper()
	old := &oldDaemon{}
	desc := oldCreateSessionDescriptor(t)
	_, handler := tymuxv1connect.NewTymuxServiceHandler(old)
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == tymuxv1connect.TymuxServiceCreateSessionProcedure {
			body, _ := io.ReadAll(r.Body)
			msg := dynamicpb.NewMessage(desc)
			if err := proto.Unmarshal(body, msg); err != nil {
				t.Errorf("old daemon cannot decode request: %v", err)
			}
			msg.SetUnknown(nil)
			body, _ = proto.Marshal(msg)
			r.Body = io.NopCloser(bytes.NewReader(body))
			r.ContentLength = int64(len(body))
		}
		handler.ServeHTTP(w, r)
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	return tymuxv1connect.NewTymuxServiceClient(srv.Client(), srv.URL), old
}

func TestDetectReturnsEmptySetWhenDaemonPredatesGetServerInfo(t *testing.T) {
	client, _ := startOldDaemon(t)
	set, err := capabilities.Detect(context.Background(), client)
	if err != nil {
		t.Fatalf("Detect against an old daemon must not fail: %v", err)
	}
	for _, c := range []string{capabilities.ArgsEnv, capabilities.StderrRedirect, capabilities.FileArgs, capabilities.StdinFile} {
		if set.Has(c) {
			t.Errorf("old daemon must not report %q", c)
		}
	}
}

// The reason to gate: an old daemon accepts the new fields and drops them.
func TestOldDaemonAcceptsButIgnoresNewCreateSessionFields(t *testing.T) {
	client, old := startOldDaemon(t)
	_, err := client.CreateSession(context.Background(), connect.NewRequest(&tymuxv1.CreateSessionRequest{
		Name:       "x",
		Command:    "/bin/true",
		Args:       []string{"kept"},
		StderrPath: "/tmp/err.log",
		StdinPath:  "/tmp/stdin.txt",
		ArgSpecs:   []*tymuxv1.ArgSpec{{Source: &tymuxv1.ArgSpec_FileContentsPath{FileContentsPath: "/tmp/prompt"}}},
	}))
	if err != nil {
		t.Fatalf("old daemon rejected the request: %v", err)
	}
	if got := old.seen.GetArgs(); len(got) != 1 || got[0] != "kept" {
		t.Errorf("old daemon lost the field it knows: args=%v", got)
	}
	if old.seen.GetStderrPath() != "" || old.seen.GetStdinPath() != "" || len(old.seen.GetArgSpecs()) != 0 {
		t.Errorf("old daemon unexpectedly saw the new fields: %v", old.seen)
	}
}
