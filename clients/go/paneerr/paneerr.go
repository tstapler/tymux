// Package paneerr classifies tymuxd's "pane has no running process" errors.
//
// tymuxd answers Attach, write and (for restored panes) capture RPCs on such a pane with
// FAILED_PRECONDITION plus a google.rpc.ErrorInfo detail (domain "tymux.v1") whose reason
// tells the two causes apart without matching the message.
package paneerr

import (
	"errors"

	"connectrpc.com/connect"
	"google.golang.org/protobuf/encoding/protowire"
)

// ErrorInfo domain and reasons; mirror crates/tymux-proto/src/lib.rs.
const (
	Domain                     = "tymux.v1"
	ReasonPaneExited           = "PANE_EXITED"
	ReasonPaneRestoredNoScreen = "PANE_RESTORED_WITHOUT_SCREEN"
	errorInfoDetailType        = "google.rpc.ErrorInfo"
)

// Reason returns the tymux.v1 ErrorInfo reason carried by a FAILED_PRECONDITION error, or ""
// for any other error (including a foreign ErrorInfo domain).
func Reason(err error) string {
	var ce *connect.Error
	if !errors.As(err, &ce) || ce.Code() != connect.CodeFailedPrecondition {
		return ""
	}
	for _, d := range ce.Details() {
		if d.Type() != errorInfoDetailType {
			continue
		}
		if reason, domain := parseErrorInfo(d.Bytes()); domain == Domain {
			return reason
		}
	}
	return ""
}

// IsPaneExited: the pane's child exited; its last screen is still readable with CapturePane.
func IsPaneExited(err error) bool { return Reason(err) == ReasonPaneExited }

// IsPaneRestoredWithoutScreen: the pane was restored from disk with no screen; call
// ReviveSession and retry.
func IsPaneRestoredWithoutScreen(err error) bool { return Reason(err) == ReasonPaneRestoredNoScreen }

// IsPaneNotRunning is true for either cause.
func IsPaneNotRunning(err error) bool { return IsPaneExited(err) || IsPaneRestoredWithoutScreen(err) }

// parseErrorInfo reads google.rpc.ErrorInfo fields reason=1 and domain=2, avoiding a
// genproto dependency for two strings.
func parseErrorInfo(b []byte) (reason, domain string) {
	for len(b) > 0 {
		num, typ, n := protowire.ConsumeTag(b)
		if n < 0 {
			return "", ""
		}
		b = b[n:]
		if typ == protowire.BytesType && (num == 1 || num == 2) {
			v, n := protowire.ConsumeBytes(b)
			if n < 0 {
				return "", ""
			}
			if num == 1 {
				reason = string(v)
			} else {
				domain = string(v)
			}
			b = b[n:]
			continue
		}
		n = protowire.ConsumeFieldValue(num, typ, b)
		if n < 0 {
			return "", ""
		}
		b = b[n:]
	}
	return reason, domain
}
