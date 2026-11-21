package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func addr(last byte) (a [20]byte) {
	a[0] = 0xaa
	a[19] = last
	return a
}

// table returns a block table of n blocks with txCount transactions each and
// hashes that differ per block.
func table(n int, txCount uint32) []blockEntry {
	t := make([]blockEntry, n)
	for i := range t {
		t[i].hash[0] = byte(i + 1)
		t[i].txCount = txCount
	}
	return t
}

func TestA1TRRoundTrip(t *testing.T) {
	dir := t.TempDir()
	blocks := table(100, 10)
	w, err := createA1TR(dir, 1, 100, 199, "erigon v3.8.1", blocks)
	if err != nil {
		t.Fatal(err)
	}
	in := []record{
		{block: 100, txIndex: 0, addr: addr(1)},
		{block: 100, txIndex: 0, addr: addr(2)},
		{block: 100, txIndex: 0, addr: addr(2)}, // duplicate: dropped
		{block: 100, txIndex: 3, addr: addr(1)},
		{block: 199, txIndex: 9, addr: addr(9)},
	}
	for _, r := range in {
		if err := w.add(r); err != nil {
			t.Fatal(err)
		}
	}
	if err := w.close(); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "trace-0000000100-0000000199.a1tr")
	if _, err := os.Stat(path + ".tmp"); !os.IsNotExist(err) {
		t.Fatalf("temporary file left behind: %v", err)
	}
	f, err := readA1TR(path)
	if err != nil {
		t.Fatal(err)
	}
	if f.chainID != 1 || f.first != 100 || f.last != 199 || f.source != "erigon v3.8.1" {
		t.Fatalf("header: %+v", f)
	}
	if len(f.blocks) != 100 || f.blocks[99] != blocks[99] {
		t.Fatalf("block table: %d entries", len(f.blocks))
	}
	want := []record{in[0], in[1], in[3], in[4]}
	if len(f.records) != len(want) {
		t.Fatalf("got %d records, want %d", len(f.records), len(want))
	}
	for i := range want {
		if f.records[i] != want[i] {
			t.Fatalf("record %d: got %+v, want %+v", i, f.records[i], want[i])
		}
	}
}

// The byte layout is the contract with the Rust importer; pin it.
func TestA1TRLayout(t *testing.T) {
	dir := t.TempDir()
	blocks := table(2, 6)
	w, err := createA1TR(dir, 1, 1, 2, "x", blocks)
	if err != nil {
		t.Fatal(err)
	}
	a := addr(7)
	if err := w.add(record{block: 2, txIndex: 5, addr: a}); err != nil {
		t.Fatal(err)
	}
	if err := w.close(); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(dir, fileName(1, 2)))
	if err != nil {
		t.Fatal(err)
	}
	var want []byte
	want = append(want, "A1TR"...)
	want = append(want, 2)
	want = binary.BigEndian.AppendUint64(want, 1) // chain id
	want = binary.BigEndian.AppendUint64(want, 1) // first block
	want = binary.BigEndian.AppendUint64(want, 2) // last block
	want = binary.BigEndian.AppendUint64(want, 1) // record count
	want = append(want, 1, 'x')
	for _, b := range blocks {
		want = append(want, b.hash[:]...)
		want = binary.BigEndian.AppendUint32(want, b.txCount)
	}
	want = binary.BigEndian.AppendUint64(want, 2)
	want = binary.BigEndian.AppendUint32(want, 5)
	want = append(want, a[:]...)
	if !bytes.Equal(data, want) {
		t.Fatalf("layout:\n got %x\nwant %x", data, want)
	}
	// The same bytes pin the reader: src/addr/trace_index.rs reads this file back.
	text, err := os.ReadFile(filepath.Join("testdata", "a1tr-layout.hex"))
	if err != nil {
		t.Fatal(err)
	}
	golden, err := hex.DecodeString(strings.Join(strings.Fields(string(text)), ""))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(data, golden) {
		t.Fatalf("layout differs from testdata/a1tr-layout.hex:\n got %x\nwant %x", data, golden)
	}
}

func TestA1TRRejects(t *testing.T) {
	w, err := createA1TR(t.TempDir(), 1, 0, 10, "", table(11, 3))
	if err != nil {
		t.Fatal(err)
	}
	defer w.abort()
	if err := w.add(record{block: 5, txIndex: 1, addr: addr(1)}); err != nil {
		t.Fatal(err)
	}
	if err := w.add(record{block: 5, txIndex: 0, addr: addr(1)}); err == nil {
		t.Fatal("out-of-order record accepted")
	}
	if err := w.add(record{block: 11, txIndex: 0, addr: addr(1)}); err == nil {
		t.Fatal("record outside the file's block range accepted")
	}
	if err := w.add(record{block: 6, txIndex: 3, addr: addr(1)}); err == nil {
		t.Fatal("tx index beyond the block's transactions accepted")
	}
	if _, err := createA1TR(t.TempDir(), 1, 0, 10, "", table(10, 3)); err == nil {
		t.Fatal("block table of the wrong length accepted")
	}
}

func TestRangeWriterAlignsAndFillsGaps(t *testing.T) {
	dir := t.TempDir()
	rw := &rangeWriter{dir: dir, chainID: 1, from: 95, to: 312, per: 100, next: 95,
		blockTable: func(first, last uint64) ([]blockEntry, error) {
			return table(int(last-first+1), 1), nil
		}}
	if err := rw.add(record{block: 250, txIndex: 0, addr: addr(1)}); err != nil {
		t.Fatal(err)
	}
	if err := rw.finish(); err != nil {
		t.Fatal(err)
	}
	for _, f := range []struct {
		first, last uint64
		records     int
	}{{95, 99, 0}, {100, 199, 0}, {200, 299, 1}, {300, 312, 0}} {
		got, err := readA1TR(filepath.Join(dir, fileName(f.first, f.last)))
		if err != nil {
			t.Fatalf("%d-%d: %v", f.first, f.last, err)
		}
		if len(got.records) != f.records || len(got.blocks) != int(f.last-f.first+1) {
			t.Fatalf("%d-%d: %d records, %d blocks", f.first, f.last, len(got.records), len(got.blocks))
		}
	}
	if rw.files != 4 || rw.records != 1 {
		t.Fatalf("files = %d, records = %d, want 4 and 1", rw.files, rw.records)
	}
}

func TestCheckNoOverlap(t *testing.T) {
	dir := t.TempDir()
	touch(t, dir, fileName(0, 99))
	touch(t, dir, fileName(100, 199))
	touch(t, dir, "notes.txt")
	if err := checkNoOverlap(dir, 0, 199, 100); err != nil {
		t.Fatalf("same boundaries rejected: %v", err)
	}
	if err := checkNoOverlap(dir, 200, 299, 100); err != nil {
		t.Fatalf("disjoint range rejected: %v", err)
	}
	if err := checkNoOverlap(dir, 95, 199, 100); err == nil {
		t.Fatal("file 0-99 overlapping a run that writes 95-99 accepted")
	}
	if err := checkNoOverlap(dir, 0, 199, 50); err == nil {
		t.Fatal("other --blocks-per-file accepted")
	}
}
