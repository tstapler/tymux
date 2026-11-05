//go:build unix

package selecting

import (
	"fmt"
	"io/fs"
	"path/filepath"
	"strings"
	"syscall"
)

func evalSymlinks(p string) (string, error) { return filepath.EvalSymlinks(p) }

func ownerUID(fi fs.FileInfo) (int, bool) {
	st, ok := fi.Sys().(*syscall.Stat_t)
	if !ok {
		return 0, false
	}
	return int(st.Uid), true
}

func untrusted(format string, a ...any) error {
	return &untrustedError{msg: fmt.Sprintf(format, a...)}
}

// checkSocketTrust vets the socket path before the token is sent to it. Best effort: it is a
// check of the path at one instant (the post-dial peer check closes the race on Linux only).
//   - Every symlink on the path must be owned by root or the caller (macOS /var and /tmp are
//     root-owned), and the socket itself must not be one.
//   - The socket and its resolved parent directory must have the same owner, who must be the
//     caller, root, or in Options.TrustedSocketOwnerUIDs (needed for --socket-group deployments
//     where the daemon runs as another user).
//   - In a world-writable parent the directory must be sticky and the socket owned by the caller
//     or a trusted uid; root alone is not enough, so a root-owned socket in /tmp is refused.
//
// Missing files are left to the dial so ENOENT can still select the TCP fallback.
func checkSocketTrust(path string, pol trustPolicy, f trustFS) error {
	clean := filepath.Clean(path)
	elems := strings.Split(strings.Trim(clean, "/"), "/")
	cur := "/"
	for i, e := range elems {
		cur = filepath.Join(cur, e)
		fi, err := f.lstat(cur)
		if err != nil {
			return nil
		}
		if fi.Mode()&fs.ModeSymlink == 0 {
			continue
		}
		if i == len(elems)-1 {
			return untrusted("%s is a symlink", cur)
		}
		if owner, ok := ownerUID(fi); !ok || !pol.allows(owner) {
			return untrusted("%s is a symlink owned by uid %d", cur, owner)
		}
	}
	dir, err := f.eval(filepath.Dir(clean))
	if err != nil {
		return nil
	}
	di, err := f.stat(dir)
	if err != nil {
		return nil
	}
	fi, err := f.lstat(filepath.Join(dir, filepath.Base(clean)))
	if err != nil {
		return nil
	}
	sockOwner, ok1 := ownerUID(fi)
	dirOwner, ok2 := ownerUID(di)
	if !ok1 || !ok2 {
		return nil
	}
	if di.Mode().Perm()&0o002 != 0 {
		if di.Mode()&fs.ModeSticky == 0 {
			return untrusted("%s is world-writable and not sticky", dir)
		}
		if !pol.allowsNonRoot(sockOwner) {
			return untrusted("%s is owned by uid %d in shared directory %s", clean, sockOwner, dir)
		}
		return nil
	}
	if sockOwner != dirOwner {
		return untrusted("%s (uid %d) and its directory %s (uid %d) have different owners", clean, sockOwner, dir, dirOwner)
	}
	if !pol.allows(dirOwner) {
		return untrusted("%s is owned by uid %d, which is not you, root or a trusted owner", clean, dirOwner)
	}
	return nil
}
