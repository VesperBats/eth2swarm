// Command erigon-trace-export writes addr1's TRACE appearances for a block
// range as A1TR v1 files, read from the call-trace indexes of an Erigon datadir
// instead of re-executing blocks with trace_block. It opens the datadir
// read-only and can run next to the node.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/erigontech/erigon/common/log/v3"
)

func main() {
	var cfg config
	var to int64
	flag.StringVar(&cfg.datadir, "datadir", "", "Erigon datadir to read (opened read-only; required)")
	flag.Uint64Var(&cfg.from, "from", 0, "first block to export (inclusive)")
	flag.Int64Var(&to, "to", -1, "last block to export (inclusive; required)")
	flag.StringVar(&cfg.out, "out", "", "output directory for trace-<first>-<last>.a1tr files (required)")
	flag.StringVar(&cfg.tmpdir, "tmpdir", "", "directory for the external sort (default <out>/.tmp)")
	flag.Uint64Var(&cfg.blocksPerFile, "blocks-per-file", 100_000, "blocks per output file; files are aligned to multiples of it")
	flag.Uint64Var(&cfg.sortBufferMB, "sort-buffer-mb", 128, "memory for one sorted run of the external sort, in MB")
	flag.BoolVar(&cfg.allowUnfinalized, "allow-unfinalized", false, "allow --to above the datadir's finalized block")
	verbosity := flag.String("verbosity", "warn", "Erigon log level: crit, error, warn, info, debug")
	flag.Parse()

	if cfg.datadir == "" || cfg.out == "" || to < 0 {
		fmt.Fprintln(os.Stderr, "usage: erigon-trace-export --datadir <dir> --from <block> --to <block> --out <dir>")
		flag.PrintDefaults()
		os.Exit(2)
	}
	cfg.to = uint64(to)

	lvl, err := log.LvlFromString(*verbosity)
	if err != nil {
		fmt.Fprintln(os.Stderr, "erigon-trace-export:", err)
		os.Exit(2)
	}
	logger := log.New()
	logger.SetHandler(log.LvlFilterHandler(lvl, log.StderrHandler))
	log.Root().SetHandler(log.LvlFilterHandler(lvl, log.StderrHandler))

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, cfg, logger); err != nil {
		fmt.Fprintln(os.Stderr, "erigon-trace-export:", err)
		os.Exit(1)
	}
}
