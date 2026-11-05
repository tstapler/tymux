//go:build linux

package selecting

import (
	"net"
	"syscall"
)

// verifyPeer checks SO_PEERCRED on the connected socket: the daemon must run as the caller, root
// or a trusted uid, whatever the path looked like when it was vetted. It fails closed.
func verifyPeer(conn net.Conn, pol trustPolicy) error {
	uc, ok := conn.(*net.UnixConn)
	if !ok {
		return untrusted("connection is not a Unix socket")
	}
	raw, err := uc.SyscallConn()
	if err != nil {
		return untrusted("cannot read peer credentials: %v", err)
	}
	var cred *syscall.Ucred
	var serr error
	if err := raw.Control(func(fd uintptr) {
		cred, serr = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	}); err != nil {
		return untrusted("cannot read peer credentials: %v", err)
	}
	if serr != nil {
		return untrusted("cannot read peer credentials: %v", serr)
	}
	if !pol.allows(int(cred.Uid)) {
		return untrusted("peer uid %d is not you, root or a trusted owner", cred.Uid)
	}
	return nil
}
