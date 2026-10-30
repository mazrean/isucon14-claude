// write-behind 永続化 (persist.go / wal.go と同じ動作・同じ WAL 形式)。
// ミューテーションは行スナップショットの upsert として積み、専用スレッドがまとめて WAL か MySQL に書く。
use crate::util::{env_duration, format_datetime};
use crate::{log_error, log_info, log_warn};
use mysql::prelude::Queryable;
use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct TableDef {
    pub name: &'static str,
    pub cols: &'static [&'static str],
    pub n_key: usize,
    pub upsert: bool,
}

// tableOrder と同じ並び (WAL の table index)
#[allow(dead_code)]
pub const T_SETTINGS: u8 = 0;
pub const T_USERS: u8 = 1;
pub const T_OWNERS: u8 = 2;
pub const T_CHAIRS: u8 = 3;
pub const T_RIDES: u8 = 4;
pub const T_COUPONS: u8 = 5;
pub const T_PAYMENT_TOKENS: u8 = 6;
pub const T_RIDE_STATUSES: u8 = 7;
pub const T_CHAIR_STATES: u8 = 8;

pub static TABLES: [TableDef; 9] = [
    TableDef { name: "settings", cols: &["name", "value"], n_key: 1, upsert: true },
    TableDef {
        name: "users",
        cols: &["id", "username", "firstname", "lastname", "date_of_birth", "access_token", "invitation_code", "created_at", "updated_at"],
        n_key: 1,
        upsert: false,
    },
    TableDef { name: "owners", cols: &["id", "name", "access_token", "chair_register_token", "created_at", "updated_at"], n_key: 1, upsert: false },
    TableDef { name: "chairs", cols: &["id", "owner_id", "name", "model", "is_active", "access_token", "created_at", "updated_at"], n_key: 1, upsert: true },
    TableDef {
        name: "rides",
        cols: &[
            "id",
            "user_id",
            "chair_id",
            "pickup_latitude",
            "pickup_longitude",
            "destination_latitude",
            "destination_longitude",
            "evaluation",
            "created_at",
            "updated_at",
        ],
        n_key: 1,
        upsert: true,
    },
    TableDef { name: "coupons", cols: &["user_id", "code", "discount", "created_at", "used_by"], n_key: 2, upsert: true },
    TableDef { name: "payment_tokens", cols: &["user_id", "token", "created_at"], n_key: 1, upsert: true },
    TableDef { name: "ride_statuses", cols: &["id", "ride_id", "status", "created_at", "app_sent_at", "chair_sent_at"], n_key: 1, upsert: false },
    TableDef { name: "chair_states", cols: &["chair_id", "latitude", "longitude", "total_distance", "location_at"], n_key: 1, upsert: true },
];

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Nil,
    Str(String),
    Int(i64),
    Bool(bool),
    /// Unix ns (Go の time.Time.UnixNano)
    Time(i64),
}

impl Val {
    #[inline]
    pub fn t_us(us: i64) -> Val {
        Val::Time(us * 1000)
    }
    #[inline]
    pub fn opt_str(s: &str) -> Val {
        if s.is_empty() { Val::Nil } else { Val::Str(s.to_string()) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    pub t: u8,
    pub key: String,
    pub vals: Vec<Val>,
}

pub struct Queue {
    pub ops: Vec<Op>,
    kick: bool,
}

pub static QUEUE: Mutex<Queue> = Mutex::new(Queue { ops: Vec::new(), kick: false });
static KICK: Condvar = Condvar::new();

pub struct WalState {
    file: Option<File>,
    buf: Vec<u8>,
}
/// Go の flushMu。WAL ファイルもこれで守る
pub static FLUSH: Mutex<WalState> = Mutex::new(WalState { file: None, buf: Vec::new() });

pub static WAL_MODE: OnceLock<bool> = OnceLock::new();
pub static WAL_PATH: OnceLock<String> = OnceLock::new();
pub static DB: OnceLock<mysql::Pool> = OnceLock::new();

pub fn wal_mode() -> bool {
    *WAL_MODE.get_or_init(|| std::env::var("PERSIST_MODE").map(|v| v == "wal").unwrap_or(false))
}
fn wal_path() -> &'static str {
    WAL_PATH.get_or_init(|| std::env::var("WAL_PATH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/home/isucon/isuride.wal".to_string()))
}

pub fn enqueue(t: u8, key: String, vals: Vec<Val>) {
    let mut q = QUEUE.lock();
    q.ops.push(Op { t, key, vals });
    if q.ops.len() >= 20000 && !q.kick {
        q.kick = true;
        KICK.notify_one();
    }
}

pub fn drop_queue() {
    QUEUE.lock().ops = Vec::new();
}

pub fn queue_len() -> usize {
    QUEUE.lock().ops.len()
}

pub fn persist_loop() {
    let interval = env_duration("PERSIST_INTERVAL", Duration::from_millis(200));
    let mut next = Instant::now() + interval;
    loop {
        {
            let mut q = QUEUE.lock();
            while !q.kick {
                if KICK.wait_until(&mut q, next).timed_out() {
                    break;
                }
            }
            q.kick = false;
        }
        let now = Instant::now();
        if now >= next {
            next += interval;
            if next <= now {
                next = now + interval;
            }
        }
        flush_persist();
    }
}

pub fn flush_persist() {
    let mut w = FLUSH.lock();
    let ops = std::mem::take(&mut QUEUE.lock().ops);
    if ops.is_empty() {
        return;
    }
    if wal_mode() {
        wal_append(&mut w, dedup_ops(ops));
        return;
    }
    // MySQL 直書きモード: flushMu (FLUSH) を持ったまま書く
    let _ = write_ops(ops);
    drop(w);
}

/// tableOrder 順に並べ、upsert テーブルは同一キーの最後のスナップショットだけ残す
pub fn dedup_ops(ops: Vec<Op>) -> Vec<Op> {
    let n = ops.len();
    let mut by_table: [Vec<Op>; 9] = Default::default();
    for op in ops {
        by_table[op.t as usize].push(op);
    }
    let mut out = Vec::with_capacity(n);
    for (ti, rows) in by_table.into_iter().enumerate() {
        if TABLES[ti].upsert && rows.len() > 1 {
            let keep: Vec<bool> = {
                let mut last: FxHashMap<&str, usize> = FxHashMap::default();
                last.reserve(rows.len());
                for (i, op) in rows.iter().enumerate() {
                    last.insert(&op.key, i);
                }
                rows.iter().enumerate().map(|(i, op)| last[op.key.as_str()] == i).collect()
            };
            for (op, k) in rows.into_iter().zip(keep) {
                if k {
                    out.push(op);
                }
            }
            continue;
        }
        out.extend(rows);
    }
    out
}

// ---------------------------------------------------------------- MySQL への書き込み

pub fn sql_quote(s: &str) -> String {
    let mut b = Vec::with_capacity(s.len() + 2);
    sql_escape(&mut b, s);
    String::from_utf8(b).unwrap_or_default()
}

fn sql_escape(out: &mut Vec<u8>, s: &str) {
    out.push(b'\'');
    for &c in s.as_bytes() {
        match c {
            0 => out.extend_from_slice(b"\\0"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x1a => out.extend_from_slice(b"\\Z"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(c),
        }
    }
    out.push(b'\'');
}

fn append_val(out: &mut Vec<u8>, v: &Val) {
    match v {
        Val::Nil => out.extend_from_slice(b"NULL"),
        Val::Str(s) => sql_escape(out, s),
        Val::Int(i) => out.extend_from_slice(itoa::Buffer::new().format(*i).as_bytes()),
        Val::Bool(b) => out.push(if *b { b'1' } else { b'0' }),
        Val::Time(ns) => {
            out.push(b'\'');
            format_datetime(ns.div_euclid(1000), out);
            out.push(b'\'');
        }
    }
}

fn sql_pre(t: &TableDef) -> String {
    format!("INSERT INTO {} ({}) VALUES ", t.name, t.cols.join(","))
}
fn sql_suf(t: &TableDef) -> String {
    let ups: Vec<String> = t.cols[t.n_key..].iter().map(|c| format!("{c}=VALUES({c})")).collect();
    format!(" ON DUPLICATE KEY UPDATE {}", ups.join(","))
}

fn build_insert(t: &TableDef, rows: &[Op]) -> String {
    let mut b: Vec<u8> = Vec::with_capacity(64 + rows.len() * 128);
    b.extend_from_slice(sql_pre(t).as_bytes());
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            b.push(b',');
        }
        b.push(b'(');
        for (j, v) in r.vals.iter().enumerate() {
            if j > 0 {
                b.push(b',');
            }
            append_val(&mut b, v);
        }
        b.push(b')');
    }
    b.extend_from_slice(sql_suf(t).as_bytes());
    String::from_utf8(b).unwrap_or_default()
}

pub fn db() -> &'static mysql::Pool {
    DB.get().expect("db not initialized")
}

pub fn db_exec(sql: &str) -> Result<(), String> {
    let mut c = db().get_conn().map_err(|e| e.to_string())?;
    c.query_drop(sql).map_err(|e| e.to_string())
}

/// dedup してテーブルごとにまとめて書く。最初のエラーを返す
pub fn write_ops(ops: Vec<Op>) -> Result<(), String> {
    let ops = dedup_ops(ops);
    let mut first_err: Option<String> = None;
    let mut start = 0;
    while start < ops.len() {
        let mut end = start + 1;
        while end < ops.len() && ops[end].t == ops[start].t {
            end += 1;
        }
        if let Err(e) = write_rows(&TABLES[ops[start].t as usize], &ops[start..end]) {
            first_err.get_or_insert(e);
        }
        start = end;
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn write_rows(t: &TableDef, rows: &[Op]) -> Result<(), String> {
    const CHUNK: usize = 500;
    let mut first_err: Option<String> = None;
    for chunk in rows.chunks(CHUNK) {
        if let Err(e) = db_exec(&build_insert(t, chunk)) {
            log_error!("persist batch failed, falling back to per-row", "table={} err=\"{}\"", t.name, e);
            for r in chunk {
                if let Err(e) = write_row(t, r) {
                    first_err.get_or_insert(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn write_row(t: &TableDef, row: &Op) -> Result<(), String> {
    let sql = build_insert(t, std::slice::from_ref(row));
    let mut err = String::new();
    for _ in 0..5 {
        match db_exec(&sql) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log_error!("persist row failed", "table={} key={} err=\"{}\"", t.name, row.key, e);
                err = e;
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(err)
}

// ---------------------------------------------------------------- WAL
// レコード = [payload 長 uint32 LE][crc32 uint32 LE][payload]。1 レコード = 1 flush 分。
// payload = uvarint(op 数) { table index, key, uvarint(列数) { tag, 値 } }

const WAL_NIL: u8 = 0;
const WAL_STRING: u8 = 1;
const WAL_INT: u8 = 2;
const WAL_FALSE: u8 = 3;
const WAL_TRUE: u8 = 4;
const WAL_TIME: u8 = 5;

#[inline]
fn put_uvarint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push(v as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}
#[inline]
fn put_varint(b: &mut Vec<u8>, v: i64) {
    let mut ux = (v as u64) << 1;
    if v < 0 {
        ux = !ux;
    }
    put_uvarint(b, ux);
}
#[inline]
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_uvarint(b, s.len() as u64);
    b.extend_from_slice(s.as_bytes());
}

pub fn encode_wal_batch(b: &mut Vec<u8>, ops: &[Op]) {
    let start = b.len();
    b.extend_from_slice(&[0u8; 8]);
    put_uvarint(b, ops.len() as u64);
    for op in ops {
        b.push(op.t);
        put_str(b, &op.key);
        put_uvarint(b, op.vals.len() as u64);
        for v in &op.vals {
            match v {
                Val::Nil => b.push(WAL_NIL),
                Val::Str(s) => {
                    b.push(WAL_STRING);
                    put_str(b, s);
                }
                Val::Int(i) => {
                    b.push(WAL_INT);
                    put_varint(b, *i);
                }
                Val::Bool(x) => b.push(if *x { WAL_TRUE } else { WAL_FALSE }),
                Val::Time(ns) => {
                    b.push(WAL_TIME);
                    put_varint(b, *ns);
                }
            }
        }
    }
    let plen = (b.len() - start - 8) as u32;
    let crc = crc32fast::hash(&b[start + 8..]);
    b[start..start + 4].copy_from_slice(&plen.to_le_bytes());
    b[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

struct Dec<'a> {
    b: &'a [u8],
    i: usize,
}

impl Dec<'_> {
    fn uvarint(&mut self) -> Result<u64, String> {
        let mut x: u64 = 0;
        let mut s = 0u32;
        for k in 0..10 {
            let Some(&c) = self.b.get(self.i + k) else { return Err("wal: short record".into()) };
            if c < 0x80 {
                if k == 9 && c > 1 {
                    return Err("wal: short record".into()); // overflow (Go の Uvarint は n<0)
                }
                self.i += k + 1;
                return Ok(x | (c as u64) << s);
            }
            x |= ((c & 0x7f) as u64) << s;
            s += 7;
        }
        Err("wal: short record".into())
    }
    fn varint(&mut self) -> Result<i64, String> {
        let ux = self.uvarint()?;
        let mut x = (ux >> 1) as i64;
        if ux & 1 != 0 {
            x = !x;
        }
        Ok(x)
    }
    fn byte1(&mut self) -> Result<u8, String> {
        let c = *self.b.get(self.i).ok_or("wal: short record")?;
        self.i += 1;
        Ok(c)
    }
    fn str(&mut self) -> Result<String, String> {
        let n = self.uvarint()?;
        if ((self.b.len() - self.i) as u64) < n {
            return Err("wal: short record".into());
        }
        let s = &self.b[self.i..self.i + n as usize];
        self.i += n as usize;
        // Go の string は任意のバイト列。ここでは UTF-8 として扱う (不正なら置換)
        Ok(String::from_utf8_lossy(s).into_owned())
    }
}

pub fn decode_wal_payload(b: &[u8]) -> Result<Vec<Op>, String> {
    let mut d = Dec { b, i: 0 };
    let n_ops = d.uvarint()?;
    let mut ops = Vec::with_capacity(n_ops.min(b.len() as u64) as usize);
    for _ in 0..n_ops {
        let ti = d.byte1()?;
        if ti as usize >= TABLES.len() {
            return Err(format!("wal: bad table index {ti}"));
        }
        let key = d.str()?;
        let n_vals = d.uvarint()?;
        let mut vals = Vec::with_capacity(n_vals.min(b.len() as u64) as usize);
        for _ in 0..n_vals {
            let tag = d.byte1()?;
            vals.push(match tag {
                WAL_NIL => Val::Nil,
                WAL_STRING => Val::Str(d.str()?),
                WAL_INT => Val::Int(d.varint()?),
                WAL_FALSE => Val::Bool(false),
                WAL_TRUE => Val::Bool(true),
                WAL_TIME => Val::Time(d.varint()?),
                _ => return Err(format!("wal: bad tag {tag}")),
            });
        }
        ops.push(Op { t: ti, key, vals });
    }
    if d.i != b.len() {
        return Err("wal: trailing bytes in record".into());
    }
    Ok(ops)
}

/// レコードを順に f に渡す。途中で切れた / 壊れたレコードに当たったらそこで止め、有効な部分の長さを返す
pub fn read_wal<R: Read>(r: R, mut f: impl FnMut(Vec<Op>) -> Result<(), String>) -> Result<u64, String> {
    let mut br = BufReader::with_capacity(1 << 20, r);
    let mut off: u64 = 0;
    let mut hdr = [0u8; 8];
    let mut payload = Vec::new();
    loop {
        match read_full(&mut br, &mut hdr) {
            Ok(true) => {}
            Ok(false) => return Ok(off),
            Err(e) => return Err(e),
        }
        let n = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        payload.resize(n, 0);
        match read_full(&mut br, &mut payload) {
            Ok(true) => {}
            Ok(false) => return Ok(off),
            Err(e) => return Err(e),
        }
        if crc32fast::hash(&payload) != u32::from_le_bytes(hdr[4..8].try_into().unwrap()) {
            log_warn!("wal: checksum mismatch, stopping replay", "offset={}", off);
            return Ok(off);
        }
        let ops = match decode_wal_payload(&payload) {
            Ok(o) => o,
            Err(e) => {
                log_warn!("wal: broken record, stopping replay", "offset={} err=\"{}\"", off, e);
                return Ok(off);
            }
        };
        f(ops)?;
        off += 8 + n as u64;
    }
}

/// buf を埋め切れたら true、EOF (途中切れ含む) なら false
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, String> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(false),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(true)
}

/// WAL の内容を MySQL に反映し、成功したら WAL を空にする
pub fn replay_wal() -> Result<(), String> {
    let mut f = match OpenOptions::new().read(true).write(true).open(wal_path()) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    if size == 0 {
        return Ok(());
    }
    let start = Instant::now();
    let mut batches = 0;
    let res = read_wal(&mut f, |ops| {
        batches += 1;
        write_ops(ops)
    });
    let valid = match res {
        Ok(v) => v,
        Err(e) => {
            // DB 反映に失敗: WAL は残す。壊れた末尾の後ろに追記しないよう有効部分の末尾までは切り詰める
            if f.seek(SeekFrom::Start(0)).is_ok() {
                if let Ok(end) = read_wal(&mut f, |_| Ok(())) {
                    if end < size {
                        let _ = f.set_len(end);
                    }
                }
            }
            return Err(format!("wal replay failed after {batches} batches: {e}"));
        }
    };
    log_info!("wal replayed", "batches={} bytes={} size={} elapsed={:?}", batches, valid, size, start.elapsed());
    f.set_len(0).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())
}

pub fn open_wal() -> Result<(), String> {
    let f = OpenOptions::new().append(true).create(true).open(wal_path()).map_err(|e| e.to_string())?;
    FLUSH.lock().file = Some(f);
    Ok(())
}

fn wal_append(w: &mut WalState, ops: Vec<Op>) {
    let WalState { file, buf } = w;
    let Some(f) = file.as_mut() else { return };
    buf.clear();
    encode_wal_batch(buf, &ops);
    if let Err(e) = f.write_all(buf) {
        log_error!("wal write failed", "err=\"{}\"", e);
    }
    if buf.capacity() > 8 << 20 {
        *buf = Vec::new();
    }
}

/// FLUSH を持った状態で呼ぶ
pub fn wal_truncate(w: &mut WalState) -> Result<(), String> {
    match w.file.as_ref() {
        None => Ok(()),
        Some(f) => f.set_len(0).map_err(|e| e.to_string()),
    }
}

/// FLUSH を持った状態で呼ぶ
pub fn wal_close(w: &mut WalState) {
    if let Some(f) = w.file.take() {
        if let Err(e) = f.sync_all() {
            log_error!("wal sync failed", "err=\"{}\"", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn test_ops() -> Vec<Op> {
        // wal_test.go の testWALOps と同じ値 (時刻は UnixNano)
        let s = |x: &str| Val::Str(x.to_string());
        let t = |sec: i64, ns: i64| Val::Time(sec * 1_000_000_000 + ns);
        vec![
            Op {
                t: T_USERS,
                key: "u1".into(),
                vals: vec![
                    s("u1"),
                    s("name"),
                    s("名前"),
                    s(""),
                    s("2000-01-01"),
                    s("tok"),
                    s("inv"),
                    t(1733652000, 123456000),
                    t(1733652000, 123456789),
                ],
            },
            Op {
                t: T_CHAIRS,
                key: "c1".into(),
                vals: vec![s("c1"), s("o1"), s("chair"), s("model"), Val::Bool(true), s("tok"), t(0, 0), t(1733650000, 1000)],
            },
            Op {
                t: T_RIDES,
                key: "r1".into(),
                vals: vec![
                    s("r1"),
                    s("u1"),
                    Val::Nil,
                    Val::Int(-100),
                    Val::Int(0),
                    Val::Int(1 << 40),
                    Val::Int(-(1 << 40)),
                    Val::Nil,
                    t(1733650000, 0),
                    t(1733650001, 0),
                ],
            },
            Op { t: T_CHAIR_STATES, key: "c1".into(), vals: vec![s("c1"), Val::Int(1), Val::Int(2), Val::Int(3), t(1733650002, 999000)] },
            Op {
                t: T_CHAIRS,
                key: "c2".into(),
                vals: vec![s("c2"), s("o1"), s("chair"), s("model"), Val::Bool(false), s("tok"), t(1, 0), t(2, 0)],
            },
        ]
    }

    fn encode_all() -> Vec<u8> {
        let ops = test_ops();
        let mut b = Vec::new();
        encode_wal_batch(&mut b, &ops[..2]);
        encode_wal_batch(&mut b, &ops[2..]);
        encode_wal_batch(&mut b, &[]);
        b
    }

    #[test]
    fn roundtrip() {
        let b = encode_all();
        let mut batches = Vec::new();
        let n = read_wal(&b[..], |o| {
            batches.push(o);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, b.len() as u64);
        let ops = test_ops();
        assert_eq!(batches, vec![ops[..2].to_vec(), ops[2..].to_vec(), vec![]]);
    }

    #[test]
    fn truncated() {
        let ops = test_ops();
        let mut first = Vec::new();
        encode_wal_batch(&mut first, &ops[..2]);
        let mut full = first.clone();
        encode_wal_batch(&mut full, &ops[2..]);
        for cut in first.len()..full.len() {
            let mut nb = 0;
            let n = read_wal(&full[..cut], |_| {
                nb += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!((n, nb), (first.len() as u64, 1));
        }
        let mut corrupt = full.clone();
        *corrupt.last_mut().unwrap() ^= 0xff;
        assert_eq!(read_wal(&corrupt[..], |_| Ok(())).unwrap(), first.len() as u64);
    }

    /// Go 実装 (wal.go) が書いたファイルとバイト単位で一致すること。
    /// GO_WAL=path を与えたときだけ比較する。RUST_WAL_OUT=path で Rust 側の出力を書き出す
    #[test]
    fn go_compat() {
        let b = encode_all();
        if let Ok(p) = std::env::var("RUST_WAL_OUT") {
            std::fs::write(p, &b).unwrap();
        }
        if let Ok(p) = std::env::var("GO_WAL") {
            let g = std::fs::read(p).unwrap();
            assert_eq!(g, b, "WAL bytes differ from Go");
        }
    }

    #[test]
    fn dedup() {
        let op = |t: u8, k: &str, v: i64| Op { t, key: k.into(), vals: vec![Val::Str(k.into()), Val::Int(v)] };
        let ops = vec![op(T_CHAIR_STATES, "c1", 1), op(T_USERS, "u1", 1), op(T_CHAIR_STATES, "c1", 2), op(T_USERS, "u1", 2)];
        let got = dedup_ops(ops.clone());
        assert_eq!(got, vec![ops[1].clone(), ops[3].clone(), ops[2].clone()]);
    }

    #[test]
    fn insert_sql() {
        let ops = test_ops();
        let s = build_insert(&TABLES[T_RIDES as usize], &ops[2..3]);
        assert_eq!(
            s,
            "INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude,evaluation,created_at,updated_at) VALUES ('r1','u1',NULL,-100,0,1099511627776,-1099511627776,NULL,'2024-12-08 09:26:40.000000','2024-12-08 09:26:41.000000') ON DUPLICATE KEY UPDATE user_id=VALUES(user_id),chair_id=VALUES(chair_id),pickup_latitude=VALUES(pickup_latitude),pickup_longitude=VALUES(pickup_longitude),destination_latitude=VALUES(destination_latitude),destination_longitude=VALUES(destination_longitude),evaluation=VALUES(evaluation),created_at=VALUES(created_at),updated_at=VALUES(updated_at)"
        );
    }
}
