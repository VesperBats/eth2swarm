package main

import (
	"fmt"
)

// blockTxRange is one block's span of Erigon txNums. Erigon gives every block
// two system txNums, the first and the last of its span; transaction i of the
// block has txNum minTxNum+1+i.
type blockTxRange struct {
	block              uint64
	minTxNum, maxTxNum uint64
}

// txNumMapper turns ascending txNums into (block, tx index). maxTxNum returns
// the last txNum of a block.
type txNumMapper struct {
	maxTxNum func(block uint64) (uint64, error)
	cur      blockTxRange
	started  bool
	lastTx   uint64
}

func newTxNumMapper(firstBlock uint64, maxTxNum func(uint64) (uint64, error)) (*txNumMapper, error) {
	m := &txNumMapper{maxTxNum: maxTxNum}
	min := uint64(0)
	if firstBlock > 0 {
		prevMax, err := maxTxNum(firstBlock - 1)
		if err != nil {
			return nil, err
		}
		min = prevMax + 1
	}
	max, err := maxTxNum(firstBlock)
	if err != nil {
		return nil, err
	}
	if max < min+1 {
		return nil, fmt.Errorf("block %d: txNum span %d-%d has no room for its two system txNums", firstBlock, min, max)
	}
	m.cur = blockTxRange{block: firstBlock, minTxNum: min, maxTxNum: max}
	return m, nil
}

// minTxNum is the first txNum the mapper accepts.
func (m *txNumMapper) minTxNum() uint64 { return m.cur.minTxNum }

// lookup maps txNum, which must not be smaller than the previous call's.
// isUserTx is false for a block's system txNums.
func (m *txNumMapper) lookup(txNum uint64) (block uint64, txIndex uint32, isUserTx bool, err error) {
	if m.started && txNum < m.lastTx {
		return 0, 0, false, fmt.Errorf("txNum %d after %d: input not ascending", txNum, m.lastTx)
	}
	if txNum < m.cur.minTxNum {
		return 0, 0, false, fmt.Errorf("txNum %d before block %d (first txNum %d)", txNum, m.cur.block, m.cur.minTxNum)
	}
	m.started, m.lastTx = true, txNum
	for txNum > m.cur.maxTxNum {
		next := m.cur.block + 1
		max, err := m.maxTxNum(next)
		if err != nil {
			return 0, 0, false, err
		}
		min := m.cur.maxTxNum + 1
		if max < min+1 {
			return 0, 0, false, fmt.Errorf("block %d: txNum span %d-%d has no room for its two system txNums", next, min, max)
		}
		m.cur = blockTxRange{block: next, minTxNum: min, maxTxNum: max}
	}
	if txNum == m.cur.minTxNum || txNum == m.cur.maxTxNum {
		return m.cur.block, 0, false, nil
	}
	idx := txNum - m.cur.minTxNum - 1
	if idx > uint64(^uint32(0)) {
		return 0, 0, false, fmt.Errorf("block %d: tx index %d does not fit u32", m.cur.block, idx)
	}
	return m.cur.block, uint32(idx), true, nil
}

// indexedAddress reports whether addr1 indexes the address: it never indexes
// addresses up to 0x…ffff (the zero address and the precompiles).
func indexedAddress(a [20]byte) bool {
	for _, b := range a[:18] {
		if b != 0 {
			return true
		}
	}
	return false
}
