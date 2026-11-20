package main

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"time"

	"github.com/c2h5oh/datasize"

	"github.com/erigontech/erigon/common/log/v3"
	"github.com/erigontech/erigon/db/datadir"
	"github.com/erigontech/erigon/db/etl"
	"github.com/erigontech/erigon/db/kv"
	"github.com/erigontech/erigon/db/kv/dbcfg"
	"github.com/erigontech/erigon/db/kv/mdbx"
	"github.com/erigontech/erigon/db/kv/temporal"
	"github.com/erigontech/erigon/db/rawdb"
	"github.com/erigontech/erigon/db/recsplit/multiencseq"
	"github.com/erigontech/erigon/db/seg"
	"github.com/erigontech/erigon/db/snapshotsync/blocksnapshots"
	"github.com/erigontech/erigon/db/snapshotsync/freezeblocks"
	dbstate "github.com/erigontech/erigon/db/state"
	"github.com/erigontech/erigon/db/version"
	"github.com/erigontech/erigon/node/ethconfig"
)

type config struct {
	datadir          string
	from, to         uint64
	out              string
	tmpdir           string
	blocksPerFile    uint64
	allowUnfinalized bool
	sortBufferMB     uint64
}

// traceIndexes are Erigon's inverted indexes of call-frame addresses: every
// frame's from and to, the top-level frame included, so their union is addr1's
// TRACE set.
var traceIndexes = []struct {
	idx       kv.InvertedIdx
	fileBase  string // <version>-<fileBase>.<fromStep>-<toStep>.ef in snapshots/idx
	keysTable string // txNum (u64 BE) -> address, DupSort; the part not yet in files
}{
	{kv.TracesFromIdx, kv.FileTracesFromIdx, kv.TblTracesFromKeys},
	{kv.TracesToIdx, kv.FileTracesToIdx, kv.TblTracesToKeys},
}

// blockChunk is how many frozen blocks one short read transaction resolves.
const blockChunk = 100_000

func sourceString() string {
	return fmt.Sprintf("erigon v%d.%d.%d", version.Major, version.Minor, version.Micro)
}

type node struct {
	raw     kv.RwDB
	snaps   *blocksnapshots.RoSnapshots
	agg     *dbstate.Aggregator
	db      *temporal.DB
	br      *freezeblocks.BlockReader
	dirs    datadir.Dirs
	chainID uint64
}

// openNode opens an Erigon datadir the way rpcdaemon does with --datadir, but
// read-only: it works next to a running node and never writes to the datadir.
// Erigon's state-file loader exits the process (os.Exit) if it finds a gap
// between the files and the database.
func openNode(ctx context.Context, path string, logger log.Logger) (*node, error) {
	dirs := datadir.Open(path)
	ok, err := dbstate.CheckSaltFilesExist(dirs)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, fmt.Errorf("%s is not an Erigon datadir (no salt files)", path)
	}
	// Without erigondb.toml, Erigon's settings resolver would create one.
	settingsPath := filepath.Join(dirs.Snap, dbstate.ERIGONDB_SETTINGS_FILE)
	if _, err := os.Stat(settingsPath); err != nil {
		return nil, fmt.Errorf("%s: %w (start the node once to create it)", settingsPath, err)
	}
	n := &node{dirs: dirs}
	n.raw, err = mdbx.New(dbcfg.ChainDB, logger).Path(dirs.Chaindata).Accede(true).Readonly(true).Open(ctx)
	if err != nil {
		return nil, fmt.Errorf("open chaindata: %w", err)
	}
	var chainName string
	if err := n.raw.View(ctx, func(tx kv.Tx) error {
		genesis, err := rawdb.ReadCanonicalHash(tx, 0)
		if err != nil {
			return err
		}
		cc, err := rawdb.ReadChainConfig(tx, genesis)
		if err != nil {
			return err
		}
		if cc == nil || cc.ChainID == nil {
			return errors.New("no chain config in chaindata")
		}
		chainName = cc.ChainName
		n.chainID = cc.ChainID.Uint64()
		return nil
	}); err != nil {
		n.close()
		return nil, err
	}
	n.snaps = blocksnapshots.NewRoSnapshots(ethconfig.BlocksFreezing{ChainName: chainName}, dirs.Snap, logger)
	n.snaps.DownloadComplete()
	if err := n.snaps.OpenFolder(); err != nil {
		n.close()
		return nil, fmt.Errorf("open block snapshots: %w", err)
	}
	n.br = freezeblocks.NewBlockReader(n.snaps)
	settings, err := dbstate.ResolveErigonDBSettings(dirs, logger, false)
	if err != nil {
		n.close()
		return nil, err
	}
	n.agg, err = dbstate.New(dirs).Logger(logger).WithErigonDBSettings(settings).Open(ctx)
	if err != nil {
		n.close()
		return nil, fmt.Errorf("open state files: %w", err)
	}
	n.db, err = temporal.New(n.raw, n.agg, n.snaps)
	if err != nil {
		n.close()
		return nil, err
	}
	if err := n.db.OpenStateSnapshots(ctx); err != nil {
		n.close()
		return nil, fmt.Errorf("open state snapshots: %w", err)
	}
	return n, nil
}

func (n *node) close() {
	if n.db != nil {
		n.db.Close() // closes raw and agg, not the block snapshots
		n.snaps.Close()
		return
	}
	if n.agg != nil {
		n.agg.Close()
	}
	if n.snaps != nil {
		n.snaps.Close()
	}
	if n.raw != nil {
		n.raw.Close()
	}
}

// blockInfo is what the export needs of one block: the last txNum of its span
// and its canonical hash.
type blockInfo struct {
	maxTxNum uint64
	hash     [32]byte
}

func readBlockInfo(ctx context.Context, br *freezeblocks.BlockReader, tx kv.Tx, block uint64) (blockInfo, error) {
	h, ok, err := br.CanonicalHash(ctx, tx, block)
	if err != nil {
		return blockInfo{}, err
	}
	if !ok {
		return blockInfo{}, fmt.Errorf("block %d is not in the datadir", block)
	}
	max, err := br.TxnumReader().Max(ctx, tx, block)
	if err != nil {
		return blockInfo{}, err
	}
	return blockInfo{maxTxNum: max, hash: h}, nil
}

// chainReader serves blockInfo without a long-lived read transaction. Blocks
// not yet frozen are read once, under the transaction that also reads the
// index tail, because the node prunes them from the database after freezing
// them. Frozen blocks come from the block files this process opened, in
// chunks, each under a short transaction of its own.
type chainReader struct {
	ctx        context.Context
	n          *node
	frozen     uint64
	tail       map[uint64]blockInfo
	chunkFirst uint64
	chunk      []blockInfo
}

func (c *chainReader) readTail(tx kv.Tx, from, to uint64) error {
	c.tail = map[uint64]blockInfo{}
	for b := max(from, c.frozen+1); b <= to; b++ {
		info, err := readBlockInfo(c.ctx, c.n.br, tx, b)
		if err != nil {
			return err
		}
		c.tail[b] = info
	}
	return nil
}

func (c *chainReader) info(block uint64) (blockInfo, error) {
	if block > c.frozen {
		info, ok := c.tail[block]
		if !ok {
			return blockInfo{}, fmt.Errorf("block %d was not read with the database tail", block)
		}
		return info, nil
	}
	if block >= c.chunkFirst && block < c.chunkFirst+uint64(len(c.chunk)) {
		return c.chunk[block-c.chunkFirst], nil
	}
	last := min(block+blockChunk-1, c.frozen)
	chunk := make([]blockInfo, 0, last-block+1)
	if err := c.n.db.View(c.ctx, func(tx kv.Tx) error {
		for b := block; b <= last; b++ {
			info, err := readBlockInfo(c.ctx, c.n.br, tx, b)
			if err != nil {
				return err
			}
			chunk = append(chunk, info)
		}
		return nil
	}); err != nil {
		return blockInfo{}, err
	}
	c.chunkFirst, c.chunk = block, chunk
	return chunk[0], nil
}

func (c *chainReader) maxTxNum(block uint64) (uint64, error) {
	info, err := c.info(block)
	return info.maxTxNum, err
}

// blockTable is the A1TR block table of first..last: each block's hash and its
// number of transactions (its txNum span less the two system txNums).
func (c *chainReader) blockTable(first, last uint64) ([]blockEntry, error) {
	prevMax := uint64(0)
	if first > 0 {
		var err error
		if prevMax, err = c.maxTxNum(first - 1); err != nil {
			return nil, err
		}
	}
	table := make([]blockEntry, 0, last-first+1)
	for b := first; b <= last; b++ {
		info, err := c.info(b)
		if err != nil {
			return nil, err
		}
		span := info.maxTxNum + 1
		if b > 0 {
			span = info.maxTxNum - prevMax
		}
		if info.maxTxNum < prevMax || span < 2 || span-2 > uint64(^uint32(0)) {
			return nil, fmt.Errorf("block %d: txNum span ending at %d after %d", b, info.maxTxNum, prevMax)
		}
		table = append(table, blockEntry{hash: info.hash, txCount: uint32(span - 2)})
		prevMax = info.maxTxNum
	}
	return table, nil
}

type openFile struct {
	d    *seg.Decompressor
	base uint64
}

func run(ctx context.Context, cfg config, logger log.Logger) error {
	if cfg.to < cfg.from {
		return fmt.Errorf("--to %d is before --from %d", cfg.to, cfg.from)
	}
	if cfg.blocksPerFile == 0 {
		return errors.New("--blocks-per-file must be positive")
	}
	if cfg.sortBufferMB == 0 {
		return errors.New("--sort-buffer-mb must be positive")
	}
	if err := os.MkdirAll(cfg.out, 0o755); err != nil {
		return err
	}
	if err := checkNoOverlap(cfg.out, cfg.from, cfg.to, cfg.blocksPerFile); err != nil {
		return err
	}
	tmpdir := cfg.tmpdir
	if tmpdir == "" {
		tmpdir = filepath.Join(cfg.out, ".tmp")
		// Registered before the collector's Close, so it runs after the sort's
		// spill files are gone.
		defer os.RemoveAll(tmpdir)
	}
	if err := os.MkdirAll(tmpdir, 0o755); err != nil {
		return err
	}

	n, err := openNode(ctx, cfg.datadir, logger)
	if err != nil {
		return err
	}
	defer n.close()

	collector := etl.NewCollector("trace-export", tmpdir, etl.NewSortableBuffer(datasize.ByteSize(cfg.sortBufferMB)*datasize.MB), logger)
	defer collector.Close()
	collector.LogLvl(log.LvlDebug)
	var key [8 + 20]byte
	collect := func(txNum uint64, addr []byte) error {
		binary.BigEndian.PutUint64(key[:8], txNum)
		copy(key[8:], addr)
		return collector.Collect(key[:], nil)
	}

	chain := &chainReader{ctx: ctx, n: n, frozen: n.snaps.BlocksAvailable()}
	start := time.Now()
	files, txFrom, txTo, err := readUnderSnapshot(ctx, cfg, n, chain, collect)
	defer func() {
		for _, f := range files {
			f.d.Close()
		}
	}()
	if err != nil {
		return err
	}

	// No read transaction is open from here on: a long-lived reader would keep
	// the node from reusing the pages it frees.
	for _, f := range files {
		t := time.Now()
		if err := scanFile(ctx, f.d, f.base, txFrom, txTo, collect); err != nil {
			return err
		}
		logf("scanned %s in %s", f.d.FileName(), time.Since(t).Round(time.Second))
	}

	mapper, err := newTxNumMapper(cfg.from, chain.maxTxNum)
	if err != nil {
		return err
	}
	w := &rangeWriter{dir: cfg.out, source: sourceString(), chainID: n.chainID, from: cfg.from, to: cfg.to, per: cfg.blocksPerFile, next: cfg.from, blockTable: chain.blockTable}
	defer w.abort()
	if err := collector.Load(nil, "", func(k, _ []byte, _ etl.CurrentTableReader, _ etl.LoadNextFunc) error {
		block, txIndex, isUserTx, err := mapper.lookup(binary.BigEndian.Uint64(k[:8]))
		if err != nil {
			return err
		}
		if !isUserTx {
			return nil
		}
		r := record{block: block, txIndex: txIndex}
		copy(r.addr[:], k[8:])
		return w.add(r)
	}, etl.TransformArgs{Quit: ctx.Done()}); err != nil {
		return err
	}
	collector.Close()
	if err := w.finish(); err != nil {
		return err
	}
	logf("wrote %d files, %d records, blocks %d-%d, in %s", w.files, w.records, cfg.from, cfg.to, time.Since(start).Round(time.Second))
	return nil
}

// readUnderSnapshot does everything that needs one consistent view of the
// database, in a single short read transaction: the checks, the blocks not yet
// frozen, the index tail not yet in files, and opening the index files to read
// later (an open file stays readable even if the node merges it away).
func readUnderSnapshot(ctx context.Context, cfg config, n *node, chain *chainReader, collect func(uint64, []byte) error) (files []openFile, txFrom, txTo uint64, err error) {
	tx, err := n.db.BeginTemporalRo(ctx)
	if err != nil {
		return nil, 0, 0, err
	}
	defer tx.Rollback()

	if !cfg.allowUnfinalized {
		fin := rawdb.ReadForkchoiceFinalizedNum(tx)
		if fin == 0 {
			return nil, 0, 0, errors.New("the datadir records no finalized block; pass --allow-unfinalized to export anyway")
		}
		if cfg.to > fin {
			return nil, 0, 0, fmt.Errorf("--to %d is above the finalized block %d", cfg.to, fin)
		}
	}
	if _, ok, err := n.br.CanonicalHash(ctx, tx, cfg.to); err != nil {
		return nil, 0, 0, err
	} else if !ok {
		return nil, 0, 0, fmt.Errorf("block %d is not in the datadir", cfg.to)
	}
	if err := checkBlockFilesCurrent(n.dirs.Snap, chain.frozen, cfg.to); err != nil {
		return nil, 0, 0, err
	}
	tailFrom := cfg.from
	if tailFrom > 0 {
		tailFrom-- // the span of block --from starts after the previous block's
	}
	if err := chain.readTail(tx, tailFrom, cfg.to); err != nil {
		return nil, 0, 0, err
	}
	if cfg.from > 0 {
		prev, err := chain.maxTxNum(cfg.from - 1)
		if err != nil {
			return nil, 0, 0, err
		}
		txFrom = prev + 1
	}
	if txTo, err = chain.maxTxNum(cfg.to); err != nil {
		return nil, 0, 0, err
	}

	aggTx := dbstate.AggTx(tx)
	defer func() {
		if err != nil {
			for _, f := range files {
				f.d.Close()
			}
			files = nil
		}
	}()
	for _, ti := range traceIndexes {
		end := uint64(0)
		for _, f := range aggTx.DbgII(ti.idx).Files() {
			if f.StartRootNum() != end {
				return files, 0, 0, fmt.Errorf("%s: visible files are not contiguous at txNum %d (%s)", ti.idx, end, f.Fullpath())
			}
			end = f.EndRootNum()
			if f.EndRootNum() <= txFrom || f.StartRootNum() > txTo {
				continue
			}
			d, err := seg.NewDecompressor(f.Fullpath())
			if err != nil {
				return files, 0, 0, fmt.Errorf("open %s (the node may have merged it away: rerun): %w", f.Fullpath(), err)
			}
			files = append(files, openFile{d: d, base: f.StartRootNum()})
		}
		if err := checkIndexFilesCurrent(n.dirs.SnapIdx, ti.fileBase, end, n.agg.StepSize()); err != nil {
			return files, 0, 0, err
		}
		// The last txNum of block --to is its closing system txNum, which addr1
		// ignores; everything before it must be indexed.
		indexedEnd, err := dbIndexedEnd(tx, ti.keysTable, end)
		if err != nil {
			return files, 0, 0, err
		}
		if indexedEnd < txTo {
			return files, 0, 0, fmt.Errorf("%s is indexed below txNum %d only, block %d needs up to %d", ti.idx, indexedEnd, cfg.to, txTo-1)
		}
		if txTo >= end {
			if err := scanDB(ctx, tx, ti.keysTable, max(txFrom, end), txTo, collect); err != nil {
				return files, 0, 0, err
			}
			logf("scanned %s from txNum %d", ti.keysTable, max(txFrom, end))
		}
	}
	return files, txFrom, txTo, nil
}

// scanFile reads one .ef file: key = address, value = the sorted txNums the
// address appears at, encoded relative to the file's first txNum.
func scanFile(ctx context.Context, d *seg.Decompressor, baseTxNum, txFrom, txTo uint64, collect func(uint64, []byte) error) error {
	d.MadvSequential()
	defer d.DisableReadAhead()

	g := d.MakeGetter()
	var seq multiencseq.SequenceReader
	var addr [20]byte
	for i := 0; g.HasNext(); i++ {
		if i%100_000 == 0 && ctx.Err() != nil {
			return ctx.Err()
		}
		k, _ := g.NextUncompressed()
		if !g.HasNext() {
			return fmt.Errorf("%s: key without a value", d.FileName())
		}
		v, _ := g.NextUncompressed()
		if len(k) != len(addr) {
			return fmt.Errorf("%s: key of %d bytes, want 20", d.FileName(), len(k))
		}
		copy(addr[:], k)
		if !indexedAddress(addr) {
			continue
		}
		seq.Reset(baseTxNum, v)
		if seq.Count() == 0 || seq.Max() < txFrom || seq.Min() > txTo {
			continue
		}
		it := seq.Iterator(int(max(txFrom, baseTxNum)))
		for it.HasNext() {
			t, err := it.Next()
			if err != nil {
				it.Close()
				return err
			}
			if t > txTo {
				break
			}
			if t < txFrom {
				continue
			}
			if err := collect(t, addr[:]); err != nil {
				it.Close()
				return err
			}
		}
		it.Close()
	}
	return nil
}

// scanDB reads the not-yet-frozen part of an index from its keys table.
func scanDB(ctx context.Context, tx kv.Tx, table string, txFrom, txTo uint64, collect func(uint64, []byte) error) error {
	c, err := tx.CursorDupSort(table)
	if err != nil {
		return err
	}
	defer c.Close()
	var from [8]byte
	binary.BigEndian.PutUint64(from[:], txFrom)
	var addr [20]byte
	i := 0
	for k, v, err := c.Seek(from[:]); ; k, v, err = c.Next() {
		if err != nil {
			return err
		}
		if k == nil {
			break
		}
		if i++; i%100_000 == 0 && ctx.Err() != nil {
			return ctx.Err()
		}
		if len(k) != 8 || len(v) != len(addr) {
			return fmt.Errorf("%s: unexpected entry (key %d bytes, value %d bytes)", table, len(k), len(v))
		}
		t := binary.BigEndian.Uint64(k)
		if t > txTo {
			break
		}
		copy(addr[:], v)
		if !indexedAddress(addr) {
			continue
		}
		if err := collect(t, addr[:]); err != nil {
			return err
		}
	}
	return nil
}

// plannedRange is the output file that holds block b.
func plannedRange(b, from, to, per uint64) (first, last uint64) {
	first = max(b-b%per, from)
	last = min(b-b%per+per-1, to)
	return first, last
}

// checkNoOverlap refuses an output directory that already holds a file
// overlapping the range under different boundaries; one with the same
// boundaries is replaced.
func checkNoOverlap(dir string, from, to, per uint64) error {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return err
	}
	for _, e := range entries {
		first, last, ok := parseFileName(e.Name())
		if !ok || last < from || first > to {
			continue
		}
		if f, l := plannedRange(first, from, to, per); first < from || last > to || f != first || l != last {
			return fmt.Errorf("%s overlaps blocks %d-%d with other boundaries; remove it or export into another directory", e.Name(), from, to)
		}
	}
	return nil
}

// rangeWriter splits the sorted records into files of blocksPerFile blocks,
// aligned to multiples of it and clipped to [from, to]. Every range gets a
// file, an empty one included, so a directory states its coverage.
type rangeWriter struct {
	dir, source                  string
	chainID, from, to, per, next uint64
	blockTable                   func(first, last uint64) ([]blockEntry, error)
	cur                          *a1trWriter
	files                        int
	records                      uint64
}

func (rw *rangeWriter) ensure(block uint64) error {
	for rw.cur == nil || block > rw.cur.last {
		if rw.cur != nil {
			if err := rw.cur.close(); err != nil {
				rw.cur = nil
				return err
			}
			rw.next = rw.cur.last + 1
			rw.records += rw.cur.count
			rw.cur = nil
			rw.files++
		}
		if rw.next > rw.to || block > rw.to {
			return fmt.Errorf("block %d beyond --to %d", block, rw.to)
		}
		first, last := plannedRange(rw.next, rw.from, rw.to, rw.per)
		table, err := rw.blockTable(first, last)
		if err != nil {
			return err
		}
		w, err := createA1TR(rw.dir, rw.chainID, first, last, rw.source, table)
		if err != nil {
			return err
		}
		rw.cur = w
	}
	return nil
}

func (rw *rangeWriter) add(r record) error {
	if err := rw.ensure(r.block); err != nil {
		return err
	}
	return rw.cur.add(r)
}

func (rw *rangeWriter) finish() error {
	if err := rw.ensure(rw.to); err != nil {
		return err
	}
	err := rw.cur.close()
	if err == nil {
		rw.files++
		rw.records += rw.cur.count
	}
	rw.cur = nil
	return err
}

func (rw *rangeWriter) abort() {
	if rw.cur != nil {
		rw.cur.abort()
		rw.cur = nil
	}
}

// dbIndexedEnd is the exclusive txNum bound up to which an index is complete:
// the end of its files, or one past the last txNum in its keys table.
func dbIndexedEnd(tx kv.Tx, table string, filesEnd uint64) (uint64, error) {
	c, err := tx.CursorDupSort(table)
	if err != nil {
		return 0, err
	}
	defer c.Close()
	k, _, err := c.Last()
	if err != nil {
		return 0, err
	}
	if len(k) != 8 {
		return filesEnd, nil
	}
	return max(filesEnd, binary.BigEndian.Uint64(k)+1), nil
}

var (
	efFileRe   = regexp.MustCompile(`^v[0-9.]+-([a-z]+)\.([0-9]+)-([0-9]+)\.ef$`)
	bodyFileRe = regexp.MustCompile(`^v[0-9.]+-([0-9]+)-([0-9]+)-bodies\.seg$`)
)

// checkIndexFilesCurrent fails when the node has built index files past the
// view this process opened. Erigon prunes the keys table only after it built
// the file, so with a stale view a range could be missing from both the files
// read and the database transaction. It also fails when it cannot find the
// opened view's last file, so a naming change disables the export, not the
// check.
func checkIndexFilesCurrent(idxDir, fileBase string, filesEnd, stepSize uint64) error {
	entries, err := os.ReadDir(idxDir)
	if err != nil {
		return err
	}
	foundEnd := filesEnd == 0
	for _, e := range entries {
		m := efFileRe.FindStringSubmatch(e.Name())
		if m == nil || m[1] != fileBase {
			continue
		}
		toStep, err := strconv.ParseUint(m[3], 10, 64)
		if err != nil {
			continue
		}
		switch end := toStep * stepSize; {
		case end > filesEnd:
			return fmt.Errorf("the node wrote %s after this export opened the datadir; rerun", e.Name())
		case end == filesEnd:
			foundEnd = true
		}
	}
	if !foundEnd {
		return fmt.Errorf("%s: no %s file ends at txNum %d, the end of the opened view; unknown file naming?", idxDir, fileBase, filesEnd)
	}
	return nil
}

// checkBlockFilesCurrent fails when the node froze blocks into new snapshot
// files after this process opened them and the export needs those blocks:
// their txNum spans may already be gone from the database. Like the index
// check, it also fails when it cannot find the opened view's last file.
func checkBlockFilesCurrent(snapDir string, blocksAvailable, toBlock uint64) error {
	if toBlock <= blocksAvailable {
		return nil
	}
	entries, err := os.ReadDir(snapDir)
	if err != nil {
		return err
	}
	foundEnd := blocksAvailable == 0
	for _, e := range entries {
		m := bodyFileRe.FindStringSubmatch(e.Name())
		if m == nil {
			continue
		}
		to, err := strconv.ParseUint(m[2], 10, 64)
		if err != nil {
			continue
		}
		switch end := to * 1000; {
		case end > blocksAvailable+1:
			return fmt.Errorf("the node wrote %s after this export opened the datadir; rerun", e.Name())
		case end == blocksAvailable+1:
			foundEnd = true
		}
	}
	if !foundEnd {
		return fmt.Errorf("%s: no bodies file ends at block %d, the end of the opened view; unknown file naming?", snapDir, blocksAvailable+1)
	}
	return nil
}

func logf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "%s "+format+"\n", append([]any{time.Now().Format("15:04:05")}, args...)...)
}
