//go:build unix

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

// lockDir takes <dir>/.export.lock for this process, refusing a directory
// another export holds. The lock goes with the process, so a killed export
// leaves none behind.
func lockDir(dir string) (func(), error) {
	path := filepath.Join(dir, ".export.lock")
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = f.Close()
		return nil, fmt.Errorf("another export holds %s (%w): one export per output directory", path, err)
	}
	return func() { _ = f.Close() }, nil
}
