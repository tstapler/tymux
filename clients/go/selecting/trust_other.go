//go:build !unix

package selecting

import "path/filepath"

func evalSymlinks(p string) (string, error) { return filepath.EvalSymlinks(p) }

// checkSocketTrust is a no-op off unix: file modes and owners there do not describe who may
// replace a socket, so the pre-dial check would only produce false refusals.
func checkSocketTrust(string, trustPolicy, trustFS) error { return nil }
