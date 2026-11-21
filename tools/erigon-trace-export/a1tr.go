package main

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
)

// A1TR v2: the TRACE appearances of one block range, read by the Rust importer
// (`addr-import --trace-index <dir>`).
//
//	header:      "A1TR" ‖ version u8 = 2 ‖ chain_id u64 BE ‖ first_block u64 BE ‖
//	             last_block u64 BE ‖ record_count u64 BE ‖
//	             source_len u8 ‖ source (the version of the Erigon library that read the index)
//	block table: one entry per block first..last: hash [32]byte ‖ tx_count u32 BE
//	record:      block u64 BE ‖ tx_index u32 BE ‖ address [20]byte   (32 bytes)
//
// Records are sorted by (block, tx_index, address), contain no duplicates, and
// every tx_index is below its block's tx_count. The block table lets the
// importer bind the file to the chain it fetched blocks from.
const (
	a1trMagic         = "A1TR"
	a1trVersion       = 2
	a1trRecordSize    = 8 + 4 + 20
	a1trBlockSize     = 32 + 4
	a1trFixedHeader   = 4 + 1 + 8 + 8 + 8 + 8 + 1
	a1trRecordCountAt = 4 + 1 + 8 + 8 + 8
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

// blockEntry is one row of the block table.
type blockEntry struct {
	hash    [32]byte
	txCount uint32
}

func fileName(first, last uint64) string {
	return fmt.Sprintf("trace-%010d-%010d.a1tr", first, last)
}

var a1trNameRe = regexp.MustCompile(`^trace-([0-9]{10})-([0-9]{10})\.a1tr$`)

// parseFileName returns the block range of an A1TR file name.
func parseFileName(name string) (first, last uint64, ok bool) {
	m := a1trNameRe.FindStringSubmatch(name)
	if m == nil {
		return 0, 0, false
	}
	first, err1 := strconv.ParseUint(m[1], 10, 64)
	last, err2 := strconv.ParseUint(m[2], 10, 64)
	return first, last, err1 == nil && err2 == nil
}

// a1trWriter writes one file. It writes to a temporary name and renames on
// close, so a directory never holds a partial file under a final name.
type a1trWriter struct {
	first, last uint64
	blocks      []blockEntry
	path, tmp   string
	f           *os.File
	w           *bufio.Writer
	prev        record
	count       uint64
}

func createA1TR(dir string, chainID, first, last uint64, source string, blocks []blockEntry) (*a1trWriter, error) {
	if last < first {
		return nil, fmt.Errorf("a1tr: last block %d before first block %d", last, first)
	}
	if uint64(len(blocks)) != last-first+1 {
		return nil, fmt.Errorf("a1tr: %d block entries for blocks %d-%d", len(blocks), first, last)
	}
	if len(source) > 255 {
		return nil, errors.New("a1tr: source string longer than 255 bytes")
	}
	path := filepath.Join(dir, fileName(first, last))
	// A name of its own (<file>.<random>.tmp), so that no other writer can
	// hold the file this one renames into place. Readers match only
	// trace-*.a1tr.
	f, err := os.CreateTemp(dir, fileName(first, last)+".*.tmp")
	if err != nil {
		return nil, err
	}
	tmp := f.Name()
	// CreateTemp makes it 0600; the published file stays readable as before.
	if err := f.Chmod(0o644); err != nil {
		_ = f.Close()
		_ = os.Remove(tmp)
		return nil, err
	}
	w := &a1trWriter{first: first, last: last, blocks: blocks, path: path, tmp: tmp, f: f, w: bufio.NewWriterSize(f, 1<<20)}
	hdr := make([]byte, 0, a1trFixedHeader+len(source))
	hdr = append(hdr, a1trMagic...)
	hdr = append(hdr, a1trVersion)
	hdr = binary.BigEndian.AppendUint64(hdr, chainID)
	hdr = binary.BigEndian.AppendUint64(hdr, first)
	hdr = binary.BigEndian.AppendUint64(hdr, last)
	hdr = binary.BigEndian.AppendUint64(hdr, 0) // record_count, patched in close
	hdr = append(hdr, byte(len(source)))
	hdr = append(hdr, source...)
	if _, err := w.w.Write(hdr); err != nil {
		w.abort()
		return nil, err
	}
	var row [a1trBlockSize]byte
	for _, b := range blocks {
		copy(row[:32], b.hash[:])
		binary.BigEndian.PutUint32(row[32:], b.txCount)
		if _, err := w.w.Write(row[:]); err != nil {
			w.abort()
			return nil, err
		}
	}
	return w, nil
}

// add appends one record. Records must arrive in strictly ascending order;
// an equal record is dropped, a smaller one is an error.
func (w *a1trWriter) add(r record) error {
	if r.block < w.first || r.block > w.last {
		return fmt.Errorf("a1tr: block %d outside file range %d-%d", r.block, w.first, w.last)
	}
	if n := w.blocks[r.block-w.first].txCount; r.txIndex >= n {
		return fmt.Errorf("a1tr: block %d has %d transactions, record has tx index %d", r.block, n, r.txIndex)
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
	var n [8]byte
	binary.BigEndian.PutUint64(n[:], w.count)
	if _, err := w.f.WriteAt(n[:], a1trRecordCountAt); err != nil {
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

type a1trFile struct {
	chainID, first, last uint64
	source               string
	blocks               []blockEntry
	records              []record
}

// readA1TR parses a whole file and checks it the way an importer would.
func readA1TR(path string) (*a1trFile, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if len(data) < a1trFixedHeader || string(data[:4]) != a1trMagic {
		return nil, errors.New("a1tr: bad magic")
	}
	if data[4] != a1trVersion {
		return nil, fmt.Errorf("a1tr: unsupported version %d", data[4])
	}
	f := &a1trFile{
		chainID: binary.BigEndian.Uint64(data[5:13]),
		first:   binary.BigEndian.Uint64(data[13:21]),
		last:    binary.BigEndian.Uint64(data[21:29]),
	}
	count := binary.BigEndian.Uint64(data[29:37])
	n := int(data[37])
	if f.last < f.first || len(data) < a1trFixedHeader+n {
		return nil, errors.New("a1tr: bad header")
	}
	f.source = string(data[a1trFixedHeader : a1trFixedHeader+n])
	body := data[a1trFixedHeader+n:]
	nBlocks := f.last - f.first + 1
	if uint64(len(body)) < nBlocks*a1trBlockSize {
		return nil, errors.New("a1tr: truncated block table")
	}
	for i := uint64(0); i < nBlocks; i++ {
		var b blockEntry
		copy(b.hash[:], body[i*a1trBlockSize:])
		b.txCount = binary.BigEndian.Uint32(body[i*a1trBlockSize+32:])
		f.blocks = append(f.blocks, b)
	}
	body = body[nBlocks*a1trBlockSize:]
	if uint64(len(body)) != count*a1trRecordSize {
		return nil, fmt.Errorf("a1tr: %d record bytes, header says %d records", len(body), count)
	}
	for i := 0; i < len(body); i += a1trRecordSize {
		var r record
		r.block = binary.BigEndian.Uint64(body[i : i+8])
		r.txIndex = binary.BigEndian.Uint32(body[i+8 : i+12])
		copy(r.addr[:], body[i+12:i+32])
		if r.block < f.first || r.block > f.last {
			return nil, fmt.Errorf("a1tr: block %d outside %d-%d", r.block, f.first, f.last)
		}
		if r.txIndex >= f.blocks[r.block-f.first].txCount {
			return nil, fmt.Errorf("a1tr: tx index %d beyond block %d's transactions", r.txIndex, r.block)
		}
		if len(f.records) > 0 && !f.records[len(f.records)-1].less(r) {
			return nil, fmt.Errorf("a1tr: records not strictly ascending at block %d", r.block)
		}
		f.records = append(f.records, r)
	}
	return f, nil
}
