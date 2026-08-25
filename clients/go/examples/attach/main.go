// Command attach proves tymux's cross-language claim (ADR-003) for the
// Attach RPC in Go — clients/go had no attach coverage at all before Epic
// 5.2, only examples/list-sessions. It mirrors
// clients/ts/examples/attach.ts's shape (connect, send pane_id, print
// received output, exit on Exited) and list-sessions/main.go's
// connection-setup shape (h2c transport, connect.WithGRPC()), plus Epic
// 1.1/2.2's resume extension: an optional -resume-from-seq flag threads a
// resume token onto the first AttachRequest exactly like
// clients/ts/examples/attach.ts's resumeFromSeq option is meant to (Story
// 5.2.1 AC1).
package main

import (
	"context"
	"crypto/tls"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"

	"connectrpc.com/connect"
	"golang.org/x/net/http2"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
)

// tymuxd listens on loopback-only plain HTTP/2 (h2c, no TLS — see the
// loopback-trust security model ADR), and is a strict gRPC server (tonic),
// so the client needs an h2c-capable transport and connect.WithGRPC().
func newClient(baseURL string) tymuxv1connect.TymuxServiceClient {
	httpClient := &http.Client{
		Transport: &http2.Transport{
			AllowHTTP: true,
			DialTLSContext: func(ctx context.Context, network, addr string, _ *tls.Config) (net.Conn, error) {
				return net.Dial(network, addr)
			},
		},
	}
	return tymuxv1connect.NewTymuxServiceClient(httpClient, baseURL, connect.WithGRPC())
}

func main() {
	resumeFlag := flag.String("resume-from-seq", "", "resume from this OutputChunk seq (Epic 2.2); omit for a fresh attach")
	flag.Parse()

	args := flag.Args()
	if len(args) != 1 {
		fmt.Fprintln(os.Stderr, "usage: attach [-resume-from-seq N] <pane_id>")
		os.Exit(1)
	}
	paneID := args[0]

	// AttachRequest.resume_from_seq is optional (proto3 oneof-wrapped
	// scalar): absent means "no resume state, full attach", and Some(0)
	// is a real, distinct value from None — so the flag's presence, not
	// just its parsed value, decides whether resumeFromSeq stays nil.
	var resumeFromSeq *uint64
	if *resumeFlag != "" {
		seq, err := strconv.ParseUint(*resumeFlag, 10, 64)
		if err != nil {
			fmt.Fprintf(os.Stderr, "invalid -resume-from-seq %q: %v\n", *resumeFlag, err)
			os.Exit(1)
		}
		resumeFromSeq = &seq
	}

	client := newClient("http://127.0.0.1:7419")
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	stream := client.Attach(ctx)
	if err := stream.Send(&tymuxv1.AttachRequest{
		Payload:       &tymuxv1.AttachRequest_PaneId{PaneId: paneID},
		ResumeFromSeq: resumeFromSeq,
	}); err != nil {
		fmt.Fprintln(os.Stderr, "attach: send pane_id failed:", err)
		os.Exit(1)
	}

	for {
		event, err := stream.Receive()
		if err != nil {
			if errors.Is(err, io.EOF) {
				return
			}
			fmt.Fprintln(os.Stderr, "attach: receive failed:", err)
			os.Exit(1)
		}

		switch payload := event.GetPayload().(type) {
		case *tymuxv1.AttachEvent_Output:
			// Field 1, unchanged pre-Epic-1.1 sibling — a pre-feature
			// daemon would only ever populate this one.
			os.Stdout.Write(payload.Output)
		case *tymuxv1.AttachEvent_OutputChunk:
			// Field 7, populated instead of Output once this client has
			// declared resume support by connecting at all against a
			// regenerated daemon (Task 2.2.1c) — same raw bytes, plus seq.
			os.Stdout.Write(payload.OutputChunk.GetData())
		case *tymuxv1.AttachEvent_Snapshot:
			// Full-screen priming event (fresh attach, or the redraw that
			// follows a GapExceeded fallback). Nothing to print here for a
			// plain scrolling example — same no-op as attach.ts, which
			// also only reads the output field.
		case *tymuxv1.AttachEvent_OutputGap:
			fmt.Fprintln(os.Stderr, "tymux: output gap, some bytes were dropped")
		case *tymuxv1.AttachEvent_GapExceeded:
			fmt.Fprintf(os.Stderr, "tymux: reconnect gap too large, resyncing (oldest available seq %d)\n", payload.GapExceeded.GetOldestAvailableSeq())
		case *tymuxv1.AttachEvent_Heartbeat:
			// Application-level keepalive; nothing to render.
		case *tymuxv1.AttachEvent_Exited:
			if code := payload.Exited.Code; code != nil {
				os.Exit(int(*code))
			}
			return
		}
	}
}
