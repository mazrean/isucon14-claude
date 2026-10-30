package main

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/crc32"
	"io"
	"log/slog"
	"os"
	"time"
)

// ローカル追記型 WAL。PERSIST_MODE=wal のとき flush は MySQL ではなくここに書き、
// 起動時にまとめて MySQL へ流し込んでから loadAll する。
//
// レコード = [payload 長 uint32][crc32 uint32][payload]。1 レコード = 1 flush 分。
// payload = uvarint(op 数) { table index, key, uvarint(列数) { tag, 値 } }

const (
	walNil byte = iota
	walString
	walInt
	walFalse
	walTrue
	walTime
)

var (
	walMode bool
	walPath string
	walFile *os.File
	walW    *bufio.Writer
	walBuf  []byte

	errWALShort = errors.New("wal: short record")
	tableIndex  = map[*tableDef]byte{}
)

func init() {
	for i, t := range tableOrder {
		tableIndex[t] = byte(i)
	}
	walMode = os.Getenv("PERSIST_MODE") == "wal"
	walPath = os.Getenv("WAL_PATH")
	if walPath == "" {
		walPath = "/home/isucon/isuride.wal"
	}
}

func appendWALString(b []byte, s string) []byte {
	b = binary.AppendUvarint(b, uint64(len(s)))
	return append(b, s...)
}

// encodeWALBatch は ops を 1 レコードとして b に追記する。
func encodeWALBatch(b []byte, ops []persistOp) []byte {
	start := len(b)
	b = append(b, 0, 0, 0, 0, 0, 0, 0, 0)
	b = binary.AppendUvarint(b, uint64(len(ops)))
	for _, op := range ops {
		b = append(b, tableIndex[op.t])
		b = appendWALString(b, op.key)
		b = binary.AppendUvarint(b, uint64(len(op.vals)))
		for _, v := range op.vals {
			switch v := v.(type) {
			case nil:
				b = append(b, walNil)
			case string:
				b = append(b, walString)
				b = appendWALString(b, v)
			case int:
				b = append(b, walInt)
				b = binary.AppendVarint(b, int64(v))
			case bool:
				if v {
					b = append(b, walTrue)
				} else {
					b = append(b, walFalse)
				}
			case time.Time:
				b = append(b, walTime)
				b = binary.AppendVarint(b, v.UnixNano())
			default:
				panic(fmt.Sprintf("wal: unsupported value type %T (table %s)", v, op.t.name))
			}
		}
	}
	payload := b[start+8:]
	binary.LittleEndian.PutUint32(b[start:], uint32(len(payload)))
	binary.LittleEndian.PutUint32(b[start+4:], crc32.ChecksumIEEE(payload))
	return b
}

type walDecoder struct {
	b []byte
	i int
}

func (d *walDecoder) uvarint() (uint64, error) {
	v, n := binary.Uvarint(d.b[d.i:])
	if n <= 0 {
		return 0, errWALShort
	}
	d.i += n
	return v, nil
}

func (d *walDecoder) varint() (int64, error) {
	v, n := binary.Varint(d.b[d.i:])
	if n <= 0 {
		return 0, errWALShort
	}
	d.i += n
	return v, nil
}

func (d *walDecoder) byte1() (byte, error) {
	if d.i >= len(d.b) {
		return 0, errWALShort
	}
	c := d.b[d.i]
	d.i++
	return c, nil
}

func (d *walDecoder) str() (string, error) {
	n, err := d.uvarint()
	if err != nil {
		return "", err
	}
	if uint64(len(d.b)-d.i) < n {
		return "", errWALShort
	}
	s := string(d.b[d.i : d.i+int(n)])
	d.i += int(n)
	return s, nil
}

func decodeWALPayload(b []byte) ([]persistOp, error) {
	d := &walDecoder{b: b}
	nOps, err := d.uvarint()
	if err != nil {
		return nil, err
	}
	ops := make([]persistOp, 0, min(nOps, uint64(len(b))))
	for range nOps {
		ti, err := d.byte1()
		if err != nil {
			return nil, err
		}
		if int(ti) >= len(tableOrder) {
			return nil, fmt.Errorf("wal: bad table index %d", ti)
		}
		key, err := d.str()
		if err != nil {
			return nil, err
		}
		nVals, err := d.uvarint()
		if err != nil {
			return nil, err
		}
		vals := make([]any, 0, min(nVals, uint64(len(b))))
		for range nVals {
			tag, err := d.byte1()
			if err != nil {
				return nil, err
			}
			switch tag {
			case walNil:
				vals = append(vals, nil)
			case walString:
				s, err := d.str()
				if err != nil {
					return nil, err
				}
				vals = append(vals, s)
			case walInt:
				v, err := d.varint()
				if err != nil {
					return nil, err
				}
				vals = append(vals, int(v))
			case walFalse:
				vals = append(vals, false)
			case walTrue:
				vals = append(vals, true)
			case walTime:
				v, err := d.varint()
				if err != nil {
					return nil, err
				}
				vals = append(vals, time.Unix(0, v).UTC())
			default:
				return nil, fmt.Errorf("wal: bad tag %d", tag)
			}
		}
		ops = append(ops, persistOp{t: tableOrder[ti], key: key, vals: vals})
	}
	if d.i != len(b) {
		return nil, errors.New("wal: trailing bytes in record")
	}
	return ops, nil
}

// readWAL はレコードを順に fn に渡す。途中で切れた / 壊れたレコードに当たったらそこで止め、
// 有効な部分の長さを返す。
func readWAL(r io.Reader, fn func([]persistOp) error) (int64, error) {
	br := bufio.NewReaderSize(r, 1<<20)
	var off int64
	var hdr [8]byte
	var payload []byte
	for {
		if _, err := io.ReadFull(br, hdr[:]); err != nil {
			if err == io.EOF || err == io.ErrUnexpectedEOF {
				return off, nil
			}
			return off, err
		}
		n := binary.LittleEndian.Uint32(hdr[:4])
		if cap(payload) < int(n) {
			payload = make([]byte, n)
		}
		payload = payload[:n]
		if _, err := io.ReadFull(br, payload); err != nil {
			if err == io.EOF || err == io.ErrUnexpectedEOF {
				return off, nil
			}
			return off, err
		}
		if crc32.ChecksumIEEE(payload) != binary.LittleEndian.Uint32(hdr[4:]) {
			slog.Warn("wal: checksum mismatch, stopping replay", "offset", off)
			return off, nil
		}
		ops, err := decodeWALPayload(payload)
		if err != nil {
			slog.Warn("wal: broken record, stopping replay", "offset", off, "err", err)
			return off, nil
		}
		if err := fn(ops); err != nil {
			return off, err
		}
		off += 8 + int64(n)
	}
}

// replayWAL は WAL の内容を MySQL に反映し、成功したら WAL を空にする。
// モードに関係なく呼ぶ (wal → mysql に切り替えたときも取りこぼさない)。
func replayWAL() error {
	f, err := os.OpenFile(walPath, os.O_RDWR, 0)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil || st.Size() == 0 {
		return err
	}
	start := time.Now()
	batches := 0
	valid, err := readWAL(f, func(ops []persistOp) error {
		batches++
		return writeOps(ops)
	})
	if err != nil {
		// DB 反映に失敗: WAL は残す (upsert なので次回もう一度流しても同じ結果)。
		// ただし壊れた末尾の後ろに追記しないよう、有効部分の末尾までは切り詰める。
		if _, serr := f.Seek(0, io.SeekStart); serr == nil {
			if end, verr := readWAL(f, func([]persistOp) error { return nil }); verr == nil && end < st.Size() {
				f.Truncate(end)
			}
		}
		return fmt.Errorf("wal replay failed after %d batches: %w", batches, err)
	}
	slog.Info("wal replayed", "batches", batches, "bytes", valid, "size", st.Size(), "elapsed", time.Since(start))
	if err := f.Truncate(0); err != nil {
		return err
	}
	return f.Sync()
}

func openWAL() error {
	f, err := os.OpenFile(walPath, os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0o644)
	if err != nil {
		return err
	}
	walFile = f
	walW = bufio.NewWriterSize(f, 1<<20)
	return nil
}

// 以下は flushMu を持った状態で呼ぶ。

func walAppend(ops []persistOp) {
	if walW == nil {
		return
	}
	walBuf = encodeWALBatch(walBuf[:0], ops)
	if _, err := walW.Write(walBuf); err != nil {
		slog.Error("wal write failed", "err", err)
		return
	}
	if err := walW.Flush(); err != nil {
		slog.Error("wal flush failed", "err", err)
	}
}

func walTruncate() error {
	if walFile == nil {
		return nil
	}
	walW.Reset(walFile)
	return walFile.Truncate(0)
}

func walClose() {
	if walFile == nil {
		return
	}
	if err := walW.Flush(); err != nil {
		slog.Error("wal flush failed", "err", err)
	}
	if err := walFile.Sync(); err != nil {
		slog.Error("wal sync failed", "err", err)
	}
	walFile.Close()
	walFile, walW = nil, nil
}
