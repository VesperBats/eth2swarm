//go:build !unix

package main

// lockDir is a no-op where flock(2) does not exist: run one export per output
// directory there.
func lockDir(string) (func(), error) { return func() {}, nil }
