//go:build !linux

package selecting

import "net"

// verifyPeer is a no-op off Linux: the standard library exposes no peer credentials there, so
// the pre-dial path check is the only defence (best effort).
func verifyPeer(net.Conn, trustPolicy) error { return nil }
