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

func sourceString() string {
	return fmt.Sprintf("erigon v%d.%d.%d", version.Major, version.Minor, version.Micro)
}

type node struct {
	raw   kv.RwDB
	snaps *blocksnapshots.RoSnapshots
	agg   *dbstate.Aggregator
	db    *temporal.DB
	br    *freezeblocks.BlockReader
	dirs  datadir.Dirs
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
		if cc == nil {
			return errors.New("no chain config in chaindata")
		}
		chainName = cc.ChainName
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

	tx, err := n.db.BeginTemporalRo(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if !cfg.allowUnfinalized {
		fin := rawdb.ReadForkchoiceFinalizedNum(tx)
		if fin == 0 {
			return errors.New("the datadir records no finalized block; pass --allow-unfinalized to export anyway")
		}
		if cfg.to > fin {
			return fmt.Errorf("--to %d is above the finalized block %d", cfg.to, fin)
		}
	}

	if _, ok, err := n.br.CanonicalHash(ctx, tx, cfg.to); err != nil {
		return err
	} else if !ok {
		return fmt.Errorf("block %d is not in the datadir", cfg.to)
	}
	if err := checkBlockFilesCurrent(n.dirs.Snap, n.snaps.BlocksAvailable(), cfg.to); err != nil {
		return err
	}

	txNums := n.br.TxnumReader()
	maxTxNum := func(block uint64) (uint64, error) {
		return txNums.Max(ctx, tx, block)
	}
	mapper, err := newTxNumMapper(cfg.from, maxTxNum)
	if err != nil {
		return err
	}
	txFrom := mapper.minTxNum()
	txTo, err := maxTxNum(cfg.to)
	if err != nil {
		return err
	}
	if txTo <= txFrom {
		return fmt.Errorf("block %d is not in the datadir", cfg.to)
	}

	aggTx := dbstate.AggTx(tx)
	filesEnd := make([]uint64, len(traceIndexes))
	for i, ti := range traceIndexes {
		end := uint64(0)
		for _, f := range aggTx.DbgII(ti.idx).Files() {
			if f.StartRootNum() != end {
				return fmt.Errorf("%s: visible files are not contiguous at txNum %d (%s)", ti.idx, end, f.Fullpath())
			}
			end = f.EndRootNum()
		}
		filesEnd[i] = end
		if err := checkIndexFilesCurrent(n.dirs.SnapIdx, ti.fileBase, end, n.agg.StepSize()); err != nil {
			return err
		}
		// The last txNum of block --to is its closing system txNum, which addr1
		// ignores; everything before it must be indexed.
		indexedEnd, err := dbIndexedEnd(tx, ti.keysTable, end)
		if err != nil {
			return err
		}
		if indexedEnd < txTo {
			return fmt.Errorf("%s is indexed below txNum %d only, block %d needs up to %d", ti.idx, indexedEnd, cfg.to, txTo-1)
		}
	}

	collector := etl.NewCollector("trace-export", tmpdir, etl.NewSortableBuffer(datasize.ByteSize(cfg.sortBufferMB)*datasize.MB), logger)
	defer collector.Close()
	collector.LogLvl(log.LvlDebug)

	var key [8 + 20]byte
	collect := func(txNum uint64, addr []byte) error {
		binary.BigEndian.PutUint64(key[:8], txNum)
		copy(key[8:], addr)
		return collector.Collect(key[:], nil)
	}

	start := time.Now()
	for i, ti := range traceIndexes {
		for _, f := range aggTx.DbgII(ti.idx).Files() {
			if f.EndRootNum() <= txFrom || f.StartRootNum() > txTo {
				continue
			}
			t := time.Now()
			if err := scanFile(ctx, f.Fullpath(), f.StartRootNum(), txFrom, txTo, collect); err != nil {
				return err
			}
			logf("scanned %s in %s", filepath.Base(f.Fullpath()), time.Since(t).Round(time.Second))
		}
		if txTo >= filesEnd[i] {
			if err := scanDB(ctx, tx, ti.keysTable, max(txFrom, filesEnd[i]), txTo, collect); err != nil {
				return err
			}
			logf("scanned %s from txNum %d", ti.keysTable, max(txFrom, filesEnd[i]))
		}
	}

	w := &rangeWriter{dir: cfg.out, source: sourceString(), from: cfg.from, to: cfg.to, per: cfg.blocksPerFile, next: cfg.from}
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

// scanFile reads one .ef file: key = address, value = the sorted txNums the
// address appears at, encoded relative to the file's first txNum.
func scanFile(ctx context.Context, path string, baseTxNum, txFrom, txTo uint64, collect func(uint64, []byte) error) error {
	d, err := seg.NewDecompressor(path)
	if err != nil {
		return fmt.Errorf("open %s (Erigon may have merged it away: rerun): %w", path, err)
	}
	defer d.Close()
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
			return fmt.Errorf("%s: key without a value", path)
		}
		v, _ := g.NextUncompressed()
		if len(k) != len(addr) {
			return fmt.Errorf("%s: key of %d bytes, want 20", path, len(k))
		}
		copy(addr[:], k)
		if !indexedAddress(addr) {
			continue
		}
		seq.Reset(baseTxNum, v)
		if seq.Count() == 0 || seq.Max() < txFrom || seq.Min() > txTo {
			continue
		}
		it := seq.Iterator(int(txFrom))
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

// rangeWriter splits the sorted records into files of blocksPerFile blocks,
// aligned to multiples of it and clipped to [from, to]. Every range gets a
// file, an empty one included, so a directory states its coverage.
type rangeWriter struct {
	dir, source         string
	from, to, per, next uint64
	cur                 *a1trWriter
	files               int
	records             uint64
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
		first := rw.next
		last := min(first-first%rw.per+rw.per-1, rw.to)
		w, err := createA1TR(rw.dir, first, last, rw.source)
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

func logf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "%s "+format+"\n", append([]any{time.Now().Format("15:04:05")}, args...)...)
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
// read and the database transaction.
func checkIndexFilesCurrent(idxDir, fileBase string, filesEnd, stepSize uint64) error {
	entries, err := os.ReadDir(idxDir)
	if err != nil {
		return err
	}
	for _, e := range entries {
		m := efFileRe.FindStringSubmatch(e.Name())
		if m == nil || m[1] != fileBase {
			continue
		}
		toStep, err := strconv.ParseUint(m[3], 10, 64)
		if err != nil {
			continue
		}
		if toStep*stepSize > filesEnd {
			return fmt.Errorf("the node wrote %s after this export opened the datadir; rerun", e.Name())
		}
	}
	return nil
}

// checkBlockFilesCurrent fails when the node froze blocks into new snapshot
// files after this process opened them and the export needs those blocks:
// their txNum spans may already be gone from the database.
func checkBlockFilesCurrent(snapDir string, blocksAvailable, toBlock uint64) error {
	if toBlock <= blocksAvailable {
		return nil
	}
	entries, err := os.ReadDir(snapDir)
	if err != nil {
		return err
	}
	for _, e := range entries {
		m := bodyFileRe.FindStringSubmatch(e.Name())
		if m == nil {
			continue
		}
		to, err := strconv.ParseUint(m[2], 10, 64)
		if err != nil {
			continue
		}
		if to*1000 > blocksAvailable+1 {
			return fmt.Errorf("the node wrote %s after this export opened the datadir; rerun", e.Name())
		}
	}
	return nil
}
