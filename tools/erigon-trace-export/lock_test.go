//go:build unix

package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestOneExportPerDirectory(t *testing.T) {
	dir := t.TempDir()
	unlock, err := lockDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := lockDir(dir); err == nil {
		t.Fatal("a second export took the same directory")
	}
	unlock()
	unlock2, err := lockDir(dir)
	if err != nil {
		t.Fatalf("the lock outlived its export: %v", err)
	}
	unlock2()
}

func TestWritersUseTheirOwnTemporaryFiles(t *testing.T) {
	dir := t.TempDir()
	blocks := []blockEntry{{txCount: 1}}
	a, err := createA1TR(dir, 1, 7, 7, "test", blocks)
	if err != nil {
		t.Fatal(err)
	}
	b, err := createA1TR(dir, 1, 7, 7, "test", blocks)
	if err != nil {
		t.Fatal(err)
	}
	if a.tmp == b.tmp {
		t.Fatalf("both writers use %s", a.tmp)
	}
	if filepath.Dir(a.tmp) != dir {
		t.Fatalf("temporary file %s outside %s", a.tmp, dir)
	}
	if err := a.close(); err != nil {
		t.Fatal(err)
	}
	b.abort()
	entries, _ := os.ReadDir(dir)
	if len(entries) != 1 || entries[0].Name() != fileName(7, 7) {
		t.Fatalf("left %v, want only %s", entries, fileName(7, 7))
	}
}
