//go:build linux

package selecting

import (
	"net"
	"syscall"
)

// verifyPeer checks SO_PEERCRED on the connected socket: the daemon must run as the caller, root
// or a trusted uid, whatever the path looked like when it was vetted.
func verifyPeer(conn net.Conn, pol trustPolicy) error {
	uc, ok := conn.(*net.UnixConn)
	if !ok {
		return nil
	}
	raw, err := uc.SyscallConn()
	if err != nil {
		return nil
	}
	var cred *syscall.Ucred
	var serr error
	if err := raw.Control(func(fd uintptr) {
		cred, serr = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	}); err != nil || serr != nil {
		return nil // credentials unavailable: the path check stands alone
	}
	if !pol.allows(int(cred.Uid)) {
		return untrusted("peer uid %d is not you, root or a trusted owner", cred.Uid)
	}
	return nil
}
