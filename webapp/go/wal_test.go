package main

import (
	"bytes"
	"reflect"
	"testing"
	"time"
)

func testWALOps() []persistOp {
	jst := time.FixedZone("JST", 9*60*60)
	return []persistOp{
		{t: tUsers, key: "u1", vals: []any{"u1", "name", "名前", "", "2000-01-01", "tok", "inv", time.Date(2024, 12, 8, 10, 0, 0, 123456000, time.UTC), time.Date(2024, 12, 8, 19, 0, 0, 123456789, jst)}},
		{t: tChairs, key: "c1", vals: []any{"c1", "o1", "chair", "model", true, "tok", time.Unix(0, 0).UTC(), time.Unix(1733650000, 1000).UTC()}},
		{t: tRides, key: "r1", vals: []any{"r1", "u1", nil, -100, 0, 1 << 40, -(1 << 40), nil, time.Unix(1733650000, 0).UTC(), time.Unix(1733650001, 0).UTC()}},
		{t: tChairStates, key: "c1", vals: []any{"c1", 1, 2, 3, time.Unix(1733650002, 999000).UTC()}},
		{t: tChairs, key: "c2", vals: []any{"c2", "o1", "chair", "model", false, "tok", time.Unix(1, 0).UTC(), time.Unix(2, 0).UTC()}},
	}
}

func checkOpsEqual(t *testing.T, got, want []persistOp) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("len = %d, want %d", len(got), len(want))
	}
	for i := range want {
		g, w := got[i], want[i]
		if g.t != w.t || g.key != w.key || len(g.vals) != len(w.vals) {
			t.Fatalf("op %d: got %+v, want %+v", i, g, w)
		}
		for j := range w.vals {
			if wt, ok := w.vals[j].(time.Time); ok {
				gt, ok := g.vals[j].(time.Time)
				if !ok || !gt.Equal(wt) || gt.Location() != time.UTC {
					t.Errorf("op %d val %d: got %v, want %v", i, j, g.vals[j], wt)
				}
				continue
			}
			if !reflect.DeepEqual(g.vals[j], w.vals[j]) {
				t.Errorf("op %d val %d: got %#v, want %#v", i, j, g.vals[j], w.vals[j])
			}
		}
	}
}

func TestWALRoundTrip(t *testing.T) {
	ops := testWALOps()
	var b []byte
	b = encodeWALBatch(b, ops[:2])
	b = encodeWALBatch(b, ops[2:])
	b = encodeWALBatch(b, nil)

	var batches [][]persistOp
	n, err := readWAL(bytes.NewReader(b), func(o []persistOp) error {
		batches = append(batches, o)
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if n != int64(len(b)) {
		t.Fatalf("valid = %d, want %d", n, len(b))
	}
	if len(batches) != 3 {
		t.Fatalf("batches = %d, want 3", len(batches))
	}
	checkOpsEqual(t, batches[0], ops[:2])
	checkOpsEqual(t, batches[1], ops[2:])
	checkOpsEqual(t, batches[2], nil)
}

func TestWALTruncatedRecord(t *testing.T) {
	ops := testWALOps()
	first := encodeWALBatch(nil, ops[:2])
	full := encodeWALBatch(append([]byte(nil), first...), ops[2:])

	// 2 レコード目の途中 (ヘッダ途中も含む) で切れていても 1 レコード目までは読める
	for cut := len(first); cut < len(full); cut++ {
		var batches [][]persistOp
		n, err := readWAL(bytes.NewReader(full[:cut]), func(o []persistOp) error {
			batches = append(batches, o)
			return nil
		})
		if err != nil {
			t.Fatalf("cut %d: %v", cut, err)
		}
		if n != int64(len(first)) || len(batches) != 1 {
			t.Fatalf("cut %d: valid = %d, batches = %d", cut, n, len(batches))
		}
		checkOpsEqual(t, batches[0], ops[:2])
	}

	// 0 埋めされた末尾 / 壊れた payload でも止まる
	broken := append(append([]byte(nil), first...), make([]byte, 64)...)
	n, _ := readWAL(bytes.NewReader(broken), func([]persistOp) error { return nil })
	if n != int64(len(first)) {
		t.Fatalf("zero tail: valid = %d, want %d", n, len(first))
	}
	corrupt := append([]byte(nil), full...)
	corrupt[len(corrupt)-1] ^= 0xff
	n, _ = readWAL(bytes.NewReader(corrupt), func([]persistOp) error { return nil })
	if n != int64(len(first)) {
		t.Fatalf("corrupt: valid = %d, want %d", n, len(first))
	}
}

func TestDedupOps(t *testing.T) {
	ops := []persistOp{
		{t: tChairStates, key: "c1", vals: []any{1}},
		{t: tUsers, key: "u1", vals: []any{1}},
		{t: tChairStates, key: "c1", vals: []any{2}},
		{t: tUsers, key: "u1", vals: []any{2}},
	}
	got := dedupOps(ops)
	want := []persistOp{ops[1], ops[3], ops[2]}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %+v, want %+v", got, want)
	}
}
