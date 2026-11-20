package main

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// A1TR v1: the TRACE appearances of one block range, read by the Rust importer
// (`addr-import --trace-index <dir>`).
//
//	header: "A1TR" ‖ version u8 = 1 ‖ first_block u64 BE ‖ last_block u64 BE ‖
//	        source_len u8 ‖ source (the version of the Erigon library that read the index)
//	record: block u64 BE ‖ tx_index u32 BE ‖ address [20]byte   (32 bytes)
//
// Records are sorted by (block, tx_index, address) and contain no duplicates.
const (
	a1trMagic      = "A1TR"
	a1trVersion    = 1
	a1trRecordSize = 8 + 4 + 20
)

type record struct {
	block   uint64
	txIndex uint32
	addr    [20]byte
}

func (r record) less(o record) bool {
	if r.block != o.block {
		return r.block < o.block
	}
	if r.txIndex != o.txIndex {
		return r.txIndex < o.txIndex
	}
	return string(r.addr[:]) < string(o.addr[:])
}

func fileName(first, last uint64) string {
	return fmt.Sprintf("trace-%010d-%010d.a1tr", first, last)
}

// a1trWriter writes one file. It writes to a temporary name and renames on
// Close, so a directory never holds a partial file under a final name.
type a1trWriter struct {
	first, last uint64
	path, tmp   string
	f           *os.File
	w           *bufio.Writer
	prev        record
	count       uint64
}

func createA1TR(dir string, first, last uint64, source string) (*a1trWriter, error) {
	if last < first {
		return nil, fmt.Errorf("a1tr: last block %d before first block %d", last, first)
	}
	if len(source) > 255 {
		return nil, fmt.Errorf("a1tr: source string longer than 255 bytes")
	}
	path := filepath.Join(dir, fileName(first, last))
	tmp := path + ".tmp"
	f, err := os.Create(tmp)
	if err != nil {
		return nil, err
	}
	w := &a1trWriter{first: first, last: last, path: path, tmp: tmp, f: f, w: bufio.NewWriterSize(f, 1<<20)}
	hdr := make([]byte, 0, 4+1+8+8+1+len(source))
	hdr = append(hdr, a1trMagic...)
	hdr = append(hdr, a1trVersion)
	hdr = binary.BigEndian.AppendUint64(hdr, first)
	hdr = binary.BigEndian.AppendUint64(hdr, last)
	hdr = append(hdr, byte(len(source)))
	hdr = append(hdr, source...)
	if _, err := w.w.Write(hdr); err != nil {
		w.abort()
		return nil, err
	}
	return w, nil
}

// add appends one record. Records must arrive in strictly ascending order;
// an equal record is dropped, a smaller one is an error.
func (w *a1trWriter) add(r record) error {
	if r.block < w.first || r.block > w.last {
		return fmt.Errorf("a1tr: block %d outside file range %d-%d", r.block, w.first, w.last)
	}
	if w.count > 0 {
		if r == w.prev {
			return nil
		}
		if r.less(w.prev) {
			return fmt.Errorf("a1tr: record out of order at block %d tx %d", r.block, r.txIndex)
		}
	}
	var buf [a1trRecordSize]byte
	binary.BigEndian.PutUint64(buf[0:8], r.block)
	binary.BigEndian.PutUint32(buf[8:12], r.txIndex)
	copy(buf[12:], r.addr[:])
	if _, err := w.w.Write(buf[:]); err != nil {
		return err
	}
	w.prev = r
	w.count++
	return nil
}

func (w *a1trWriter) close() error {
	if err := w.w.Flush(); err != nil {
		w.abort()
		return err
	}
	if err := w.f.Sync(); err != nil {
		w.abort()
		return err
	}
	if err := w.f.Close(); err != nil {
		_ = os.Remove(w.tmp)
		return err
	}
	return os.Rename(w.tmp, w.path)
}

func (w *a1trWriter) abort() {
	_ = w.f.Close()
	_ = os.Remove(w.tmp)
}

// readA1TR parses a whole file and checks its order.
func readA1TR(path string) (first, last uint64, source string, recs []record, err error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0, 0, "", nil, err
	}
	if len(data) < 4+1+8+8+1 || string(data[:4]) != a1trMagic {
		return 0, 0, "", nil, errors.New("a1tr: bad magic")
	}
	if data[4] != a1trVersion {
		return 0, 0, "", nil, fmt.Errorf("a1tr: unsupported version %d", data[4])
	}
	first = binary.BigEndian.Uint64(data[5:13])
	last = binary.BigEndian.Uint64(data[13:21])
	n := int(data[21])
	if len(data) < 22+n {
		return 0, 0, "", nil, errors.New("a1tr: truncated header")
	}
	source = string(data[22 : 22+n])
	body := data[22+n:]
	if len(body)%a1trRecordSize != 0 {
		return 0, 0, "", nil, errors.New("a1tr: truncated record")
	}
	for i := 0; i < len(body); i += a1trRecordSize {
		var r record
		r.block = binary.BigEndian.Uint64(body[i : i+8])
		r.txIndex = binary.BigEndian.Uint32(body[i+8 : i+12])
		copy(r.addr[:], body[i+12:i+32])
		if r.block < first || r.block > last {
			return 0, 0, "", nil, fmt.Errorf("a1tr: block %d outside %d-%d", r.block, first, last)
		}
		if len(recs) > 0 && !recs[len(recs)-1].less(r) {
			return 0, 0, "", nil, fmt.Errorf("a1tr: records not strictly ascending at block %d", r.block)
		}
		recs = append(recs, r)
	}
	return first, last, source, recs, nil
}
