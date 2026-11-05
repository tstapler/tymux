//go:build !unix

package selecting

import "io/fs"

func ownerUID(fs.FileInfo) (int, bool) { return 0, false }
