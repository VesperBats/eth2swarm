package main

import (
	"os"
	"path/filepath"
	"testing"
)

func touch(t *testing.T, dir, name string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, name), nil, 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestCheckIndexFilesCurrent(t *testing.T) {
	dir := t.TempDir()
	touch(t, dir, "v3.0-tracesfrom.0-256.ef")
	touch(t, dir, "v3.0-tracesfrom.256-260.ef")
	touch(t, dir, "v3.0-tracesto.0-300.ef") // another index: ignored
	touch(t, dir, "v3.0-tracesfrom.0-256.efi")
	const step = 10
	if err := checkIndexFilesCurrent(dir, "tracesfrom", 260*step, step); err != nil {
		t.Fatalf("current view rejected: %v", err)
	}
	if err := checkIndexFilesCurrent(dir, "tracesfrom", 256*step, step); err == nil {
		t.Fatal("a file past the opened view was not reported")
	}
	if err := checkIndexFilesCurrent(dir, "tracesfrom", 258*step, step); err == nil {
		t.Fatal("a view whose last file is not on disk was accepted")
	}
	if err := checkIndexFilesCurrent(t.TempDir(), "tracesfrom", 0, step); err != nil {
		t.Fatalf("empty view rejected: %v", err)
	}
}

func TestCheckBlockFilesCurrent(t *testing.T) {
	dir := t.TempDir()
	touch(t, dir, "v1.1-026451-026452-bodies.seg")
	touch(t, dir, "v1.1-026452-026453-headers.seg") // not a bodies file: ignored
	// The opened view ends with the bodies file that ends at block 26452000.
	if err := checkBlockFilesCurrent(dir, 26_451_999, 26_460_000); err != nil {
		t.Fatalf("current view rejected: %v", err)
	}
	if err := checkBlockFilesCurrent(dir, 26_449_999, 26_460_000); err == nil {
		t.Fatal("a view whose last file is not on disk was accepted")
	}
	touch(t, dir, "v1.1-026452-026453-bodies.seg")
	if err := checkBlockFilesCurrent(dir, 26_451_999, 26_460_000); err == nil {
		t.Fatal("a bodies file past the opened view was not reported")
	}
	// A range inside the frozen blocks does not depend on the database.
	if err := checkBlockFilesCurrent(dir, 26_451_999, 26_000_000); err != nil {
		t.Fatalf("range inside the view rejected: %v", err)
	}
}
