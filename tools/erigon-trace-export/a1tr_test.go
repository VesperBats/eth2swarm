package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func addr(last byte) (a [20]byte) {
	a[0] = 0xaa
	a[19] = last
	return a
}

func TestA1TRRoundTrip(t *testing.T) {
	dir := t.TempDir()
	w, err := createA1TR(dir, 100, 199, "erigon v3.8.1")
	if err != nil {
		t.Fatal(err)
	}
	in := []record{
		{block: 100, txIndex: 0, addr: addr(1)},
		{block: 100, txIndex: 0, addr: addr(2)},
		{block: 100, txIndex: 0, addr: addr(2)}, // duplicate: dropped
		{block: 100, txIndex: 3, addr: addr(1)},
		{block: 199, txIndex: 1, addr: addr(9)},
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
	first, last, source, got, err := readA1TR(path)
	if err != nil {
		t.Fatal(err)
	}
	if first != 100 || last != 199 || source != "erigon v3.8.1" {
		t.Fatalf("header: %d %d %q", first, last, source)
	}
	want := []record{in[0], in[1], in[3], in[4]}
	if len(got) != len(want) {
		t.Fatalf("got %d records, want %d", len(got), len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("record %d: got %+v, want %+v", i, got[i], want[i])
		}
	}
}

// The byte layout is the contract with the Rust importer; pin it.
func TestA1TRLayout(t *testing.T) {
	dir := t.TempDir()
	w, err := createA1TR(dir, 1, 2, "x")
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
	want = append(want, 1)
	want = binary.BigEndian.AppendUint64(want, 1)
	want = binary.BigEndian.AppendUint64(want, 2)
	want = append(want, 1, 'x')
	want = binary.BigEndian.AppendUint64(want, 2)
	want = binary.BigEndian.AppendUint32(want, 5)
	want = append(want, a[:]...)
	if !bytes.Equal(data, want) {
		t.Fatalf("layout:\n got %x\nwant %x", data, want)
	}
}

func TestA1TRRejectsDisorder(t *testing.T) {
	w, err := createA1TR(t.TempDir(), 0, 10, "")
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
}

func TestRangeWriterAlignsAndFillsGaps(t *testing.T) {
	dir := t.TempDir()
	rw := &rangeWriter{dir: dir, from: 95, to: 312, per: 100, next: 95}
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
		_, _, _, recs, err := readA1TR(filepath.Join(dir, fileName(f.first, f.last)))
		if err != nil {
			t.Fatalf("%d-%d: %v", f.first, f.last, err)
		}
		if len(recs) != f.records {
			t.Fatalf("%d-%d: %d records, want %d", f.first, f.last, len(recs), f.records)
		}
	}
	if rw.files != 4 || rw.records != 1 {
		t.Fatalf("files = %d, records = %d, want 4 and 1", rw.files, rw.records)
	}
}
