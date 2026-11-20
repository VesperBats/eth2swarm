package main

import "testing"

// Three blocks: block 10 with 2 transactions, block 11 with none, block 12 with
// 1. Each block's span starts and ends with a system txNum.
//
//	block 10: txNums 100 (sys) 101 (tx0) 102 (tx1) 103 (sys)
//	block 11: txNums 104 (sys) 105 (sys)
//	block 12: txNums 106 (sys) 107 (tx0) 108 (sys)
func testMaxTxNum(block uint64) (uint64, error) {
	return map[uint64]uint64{9: 99, 10: 103, 11: 105, 12: 108}[block], nil
}

func TestTxNumMapper(t *testing.T) {
	m, err := newTxNumMapper(10, testMaxTxNum)
	if err != nil {
		t.Fatal(err)
	}
	if m.minTxNum() != 100 {
		t.Fatalf("minTxNum = %d, want 100", m.minTxNum())
	}
	for _, c := range []struct {
		txNum   uint64
		block   uint64
		txIndex uint32
		user    bool
	}{
		{100, 10, 0, false},
		{101, 10, 0, true},
		{101, 10, 0, true}, // repeated txNum is fine
		{102, 10, 1, true},
		{103, 10, 0, false},
		{105, 11, 0, false},
		{107, 12, 0, true},
		{108, 12, 0, false},
	} {
		block, idx, user, err := m.lookup(c.txNum)
		if err != nil {
			t.Fatalf("txNum %d: %v", c.txNum, err)
		}
		if user != c.user || (user && (block != c.block || idx != c.txIndex)) {
			t.Fatalf("txNum %d: got (%d, %d, %v), want (%d, %d, %v)", c.txNum, block, idx, user, c.block, c.txIndex, c.user)
		}
	}
	if _, _, _, err := m.lookup(102); err == nil {
		t.Fatal("descending txNum accepted")
	}
}

func TestTxNumMapperRejectsTxBeforeRange(t *testing.T) {
	m, err := newTxNumMapper(10, testMaxTxNum)
	if err != nil {
		t.Fatal(err)
	}
	if _, _, _, err := m.lookup(99); err == nil {
		t.Fatal("txNum before the first block accepted")
	}
}

func TestIndexedAddress(t *testing.T) {
	var a [20]byte
	if indexedAddress(a) {
		t.Fatal("zero address indexed")
	}
	a[18], a[19] = 0xff, 0xff
	if indexedAddress(a) {
		t.Fatal("0x…ffff indexed")
	}
	a[17] = 1
	if !indexedAddress(a) {
		t.Fatal("0x…01ffff not indexed")
	}
}
