// Command list-sessions proves tymux's cross-language claim (ADR-003) for
// Go: a client generated straight from proto/tymux/v1/tymux.proto via buf
// + protoc-gen-go + protoc-gen-connect-go, driving a real tymuxd daemon's
// ListSessions RPC — no Rust code involved. Mirrors
// clients/ts/examples/list-sessions.ts's shape and output.
package main

import (
	"context"
	"fmt"
	"os"

	"connectrpc.com/connect"

	tymuxv1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// flattenPaneIDs walks a window's layout tree in tree order, returning
// every leaf pane_id — same traversal as list-sessions.ts's flattenPaneIds.
func flattenPaneIDs(layout *tymuxv1.Layout) []string {
	if layout == nil {
		return nil
	}
	switch node := layout.GetNode().(type) {
	case *tymuxv1.Layout_Pane:
		return []string{node.Pane.GetId()}
	case *tymuxv1.Layout_Split:
		var ids []string
		for _, child := range node.Split.GetChildren() {
			ids = append(ids, flattenPaneIDs(child.GetLayout())...)
		}
		return ids
	default:
		return nil
	}
}

func main() {
	client, err := udsdialer.DialWithFallback(os.Getenv("TYMUXD_TOKEN"))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	resp, err := client.ListSessions(context.Background(), connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
	if err != nil {
		fmt.Fprintln(os.Stderr, "ListSessions failed:", err)
		os.Exit(1)
	}

	for _, session := range resp.Msg.GetSessions() {
		fmt.Printf("%s\t%s\t%s\n", session.GetId(), session.GetName(), session.GetLiveness())
		for _, window := range session.GetWindows() {
			for _, paneID := range flattenPaneIDs(window.GetLayout()) {
				fmt.Printf("  window %s (%s)\tpane %s\n", window.GetName(), window.GetId(), paneID)
			}
		}
	}
}
