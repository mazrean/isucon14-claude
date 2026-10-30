// 全データをメモリに持つ。ロックは単一の STATE で守る (store.go / notify.go 相当)。
use crate::persist::{self, Val};
use crate::util::{J, new_id, parse_datetime, us_to_ms, wall_us};
use bytes::Bytes;
use mysql::Value;
use mysql::prelude::Queryable;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, LazyLock};

pub const INITIAL_FARE: i64 = 500;
pub const FARE_PER_DISTANCE: i64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum St {
    Matching,
    Enroute,
    Pickup,
    Carrying,
    Arrived,
    Completed,
}

impl St {
    pub fn as_str(self) -> &'static str {
        match self {
            St::Matching => "MATCHING",
            St::Enroute => "ENROUTE",
            St::Pickup => "PICKUP",
            St::Carrying => "CARRYING",
            St::Arrived => "ARRIVED",
            St::Completed => "COMPLETED",
        }
    }
    pub fn parse(s: &str) -> Option<St> {
        Some(match s {
            "MATCHING" => St::Matching,
            "ENROUTE" => St::Enroute,
            "PICKUP" => St::Pickup,
            "CARRYING" => St::Carrying,
            "ARRIVED" => St::Arrived,
            "COMPLETED" => St::Completed,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------- 通知キュー

pub struct NotifInner {
    pub queue: Vec<Bytes>,
    pub last: Option<Bytes>,
    pub generation: u64,
    /// 現在の通知ストリーム (SSE のレスポンスボディ)。繋がっている間は push が直接書き込む
    tx: Option<tokio::sync::mpsc::UnboundedSender<Bytes>>,
}

/// SSE の 1 イベント分 ("data: <json>\n\n")
fn sse_frame(out: &mut Vec<u8>, ev: &[u8]) {
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(ev);
    out.extend_from_slice(b"\n\n");
}

/// ユーザー / 椅子ごとの通知キュー (Go の Notifier)。push は STATE を持って行う
pub struct Notifier {
    pub inner: Mutex<NotifInner>,
}

impl Notifier {
    pub fn new() -> Arc<Notifier> {
        Arc::new(Notifier { inner: Mutex::new(NotifInner { queue: Vec::new(), last: None, generation: 0, tx: None }) })
    }
    /// 通知ストリームが繋がっていればそこへ直接書き (中継タスクを起こさない)、無ければ次の接続まで溜める
    pub fn push(&self, ev: Bytes) {
        let mut i = self.inner.lock();
        if let Some(tx) = &i.tx {
            let mut b = Vec::with_capacity(ev.len() + 8);
            sse_frame(&mut b, &ev);
            if tx.send(Bytes::from(b)).is_ok() {
                i.last = Some(ev);
                return;
            }
            i.tx = None;
        }
        i.queue.push(ev);
    }
    /// 新しい通知ストリームを張る。前の接続は閉じ (ボディが終わる)、未送信分 (無ければ最新) から送る
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Bytes> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut i = self.inner.lock();
        i.generation += 1;
        i.tx = None;
        let evs = i.take(true);
        if !evs.is_empty() {
            let mut b = Vec::with_capacity(1024);
            for ev in &evs {
                sse_frame(&mut b, ev);
            }
            let _ = tx.send(Bytes::from(b));
        }
        i.tx = Some(tx);
        rx
    }
    /// 既存の接続を全て閉じる (新しいライドの作成時に呼ぶ)。
    /// ベンチはライドごとに通知ストリームを張り直すので、作成時点で生きている接続は前のライドのもの。
    /// それが新しいライドの ENROUTE を受け取って閉じた接続に書くと、ベンチは ENROUTE を受け取れず、
    /// 前のライドの検証済みイベントと比較して「配車位置が一致しません」になる。
    pub fn expire_streams(&self) {
        let mut i = self.inner.lock();
        i.generation += 1;
        i.tx = None;
    }
    pub fn reset(&self, ev: Bytes) {
        let mut i = self.inner.lock();
        i.queue = Vec::new();
        i.last = Some(ev);
    }
    /// 終了時: 接続を閉じる
    pub fn close(&self) {
        self.inner.lock().tx = None;
    }
}

impl NotifInner {
    /// 送るべきイベントを取り出す。first なら未送信が無くても最新を返す
    pub fn take(&mut self, first: bool) -> Vec<Bytes> {
        if self.queue.is_empty() {
            if first {
                if let Some(l) = &self.last {
                    return vec![l.clone()];
                }
            }
            return Vec::new();
        }
        let evs = std::mem::take(&mut self.queue);
        self.last = evs.last().cloned();
        evs
    }
}

// ---------------------------------------------------------------- エンティティ

pub struct Coupon {
    pub user_id: String,
    pub code: String,
    pub discount: i64,
    pub created_at: i64,
    pub used_by: String,
}

pub struct User {
    pub id: String,
    pub username: String,
    pub firstname: String,
    pub lastname: String,
    pub date_of_birth: String,
    pub access_token: String,
    pub invitation_code: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub payment_token: String,
    pub coupons: Vec<Coupon>,
    pub rides: Vec<u32>,
    pub notifier: Arc<Notifier>,
}

pub struct Owner {
    pub id: String,
    pub name: String,
    pub access_token: String,
    pub chair_register_token: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub chairs: Vec<u32>,
}

pub struct Chair {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub model: String,
    pub is_active: bool,
    pub access_token: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub owner: Option<u32>,
    pub speed: i64,
    pub has_location: bool,
    pub latitude: i64,
    pub longitude: i64,
    pub location_at: i64,
    pub total_distance: i64,
    pub total_distance_updated_at: i64,
    pub last_ride: Option<u32>,
    pub busy: bool,
    pub completed_count: i64,
    pub evaluation_sum: i64,
    pub completed: Vec<u32>,
    pub nearby_granted: i64,
    pub nearby_grant_at: i64,
    pub notifier: Arc<Notifier>,
    /// free_chairs 内の位置
    pub free_pos: u32,
}

pub const NOT_FREE: u32 = u32::MAX;

pub struct RideStatus {
    pub id: String,
    pub status: St,
    pub created_at: i64,
}

pub struct Ride {
    pub id: String,
    pub user_id: String,
    pub chair_id: String,
    pub pickup_latitude: i64,
    pub pickup_longitude: i64,
    pub destination_latitude: i64,
    pub destination_longitude: i64,
    pub evaluation: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub user: Option<u32>,
    pub chair: Option<u32>,
    pub statuses: Vec<RideStatus>,
    pub discount: i64,
    pub evaluating: bool,
    pub matched_at: i64,
    pub eval_start_at: i64,
}

#[inline]
pub fn calc_distance(a_lat: i64, a_lon: i64, b_lat: i64, b_lon: i64) -> i64 {
    (a_lat.wrapping_sub(b_lat)).wrapping_abs().wrapping_add((a_lon.wrapping_sub(b_lon)).wrapping_abs())
}

pub fn calculate_fare(plat: i64, plon: i64, dlat: i64, dlon: i64) -> i64 {
    INITIAL_FARE + FARE_PER_DISTANCE.wrapping_mul(calc_distance(plat, plon, dlat, dlon))
}

impl Ride {
    #[inline]
    pub fn status(&self) -> St {
        self.statuses.last().map(|s| s.status).unwrap_or(St::Matching)
    }
    pub fn fare(&self) -> i64 {
        let metered = FARE_PER_DISTANCE.wrapping_mul(calc_distance(self.pickup_latitude, self.pickup_longitude, self.destination_latitude, self.destination_longitude));
        INITIAL_FARE + (metered - self.discount).max(0)
    }
    pub fn sale(&self) -> i64 {
        calculate_fare(self.pickup_latitude, self.pickup_longitude, self.destination_latitude, self.destination_longitude)
    }
}

// ---------------------------------------------------------------- 状態

#[derive(Default, Clone, Copy)]
pub struct LockTimer {
    pub max_us: i64,
    pub sum_us: i64,
    pub n: i64,
}

impl LockTimer {
    pub fn add(&mut self, us: i64) {
        self.sum_us += us;
        self.n += 1;
        self.max_us = self.max_us.max(us);
    }
}

pub struct Waiter {
    pub lat: i64,
    pub lon: i64,
    pub dist: i64,
    pub tx: Option<tokio::sync::oneshot::Sender<Vec<u8>>>,
}

#[derive(Default)]
pub struct State {
    /// loadAll ごとに増える。ロック外に持ち出した index の無効化検出用
    pub epoch: u64,
    pub payment_gateway_url: String,

    pub users: Vec<User>,
    pub users_by_id: FxHashMap<String, u32>,
    pub users_by_token: FxHashMap<String, u32>,
    pub users_by_invitation: FxHashMap<String, u32>,
    pub invitation_use_count: FxHashMap<String, i64>,

    pub owners: Vec<Owner>,
    pub owners_by_id: FxHashMap<String, u32>,
    pub owners_by_token: FxHashMap<String, u32>,
    pub owners_by_register: FxHashMap<String, u32>,
    pub owner_names: FxHashSet<String>,

    pub chairs: Vec<Chair>,
    pub chairs_by_id: FxHashMap<String, u32>,
    pub chairs_by_token: FxHashMap<String, u32>,
    /// active・未割り当て・位置あり の椅子
    pub free_chairs: Vec<u32>,

    pub rides: Vec<Ride>,
    pub rides_by_id: FxHashMap<String, u32>,
    pub waiting_rides: Vec<u32>,
    pub chair_model_speed: FxHashMap<String, i64>,

    // 以下は resetStore で消えない
    pub nearby_waiters: Vec<Waiter>,
    pub last_now: i64,
    pub last_completed_ms: i64,

    pub lock_stat: [LockTimer; 3],
    pub timeline_sum: [f64; 7],
    pub timeline_n: i64,
}

pub static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

impl State {
    fn reset_store(&mut self) {
        self.users = Vec::new();
        self.users_by_id = Default::default();
        self.users_by_token = Default::default();
        self.users_by_invitation = Default::default();
        self.invitation_use_count = Default::default();
        self.owners = Vec::new();
        self.owners_by_id = Default::default();
        self.owners_by_token = Default::default();
        self.owners_by_register = Default::default();
        self.owner_names = Default::default();
        self.chairs = Vec::new();
        self.chairs_by_id = Default::default();
        self.chairs_by_token = Default::default();
        self.free_chairs = Vec::new();
        self.rides = Vec::new();
        self.rides_by_id = Default::default();
        self.waiting_rides = Vec::new();
        self.chair_model_speed = Default::default();
        self.epoch += 1;
    }

    /// 単調増加する時刻 (µs)
    pub fn now(&mut self) -> i64 {
        let mut t = wall_us();
        if t <= self.last_now {
            t = self.last_now + 1;
        }
        self.last_now = t;
        t
    }

    /// 完了時刻はミリ秒単位で一意にする
    pub fn completion_time(&mut self) -> i64 {
        let mut t = self.now();
        if us_to_ms(t) <= self.last_completed_ms {
            t = (self.last_completed_ms + 1) * 1000;
            self.last_now = t;
        }
        self.last_completed_ms = us_to_ms(t);
        t
    }

    /// 椅子の状態が変わったら呼ぶ
    pub fn update_free(&mut self, ci: u32) {
        let c = &self.chairs[ci as usize];
        let want = c.is_active && !c.busy && c.has_location && c.speed > 0;
        let pos = c.free_pos;
        if want {
            if pos == NOT_FREE {
                self.chairs[ci as usize].free_pos = self.free_chairs.len() as u32;
                self.free_chairs.push(ci);
            }
        } else if pos != NOT_FREE {
            self.free_chairs.swap_remove(pos as usize);
            if let Some(&moved) = self.free_chairs.get(pos as usize) {
                self.chairs[moved as usize].free_pos = pos;
            }
            self.chairs[ci as usize].free_pos = NOT_FREE;
        }
    }

    pub fn add_user(&mut self, u: User) -> u32 {
        let idx = self.users.len() as u32;
        self.users_by_id.insert(u.id.clone(), idx);
        self.users_by_token.insert(u.access_token.clone(), idx);
        self.users_by_invitation.insert(u.invitation_code.clone(), idx);
        self.users.push(u);
        idx
    }

    pub fn add_owner(&mut self, o: Owner) -> u32 {
        let idx = self.owners.len() as u32;
        self.owners_by_id.insert(o.id.clone(), idx);
        self.owners_by_token.insert(o.access_token.clone(), idx);
        self.owners_by_register.insert(o.chair_register_token.clone(), idx);
        self.owner_names.insert(o.name.clone());
        self.owners.push(o);
        idx
    }

    pub fn add_chair(&mut self, mut c: Chair) -> u32 {
        let idx = self.chairs.len() as u32;
        c.owner = self.owners_by_id.get(&c.owner_id).copied();
        c.speed = self.chair_model_speed.get(&c.model).copied().unwrap_or(0);
        self.chairs_by_id.insert(c.id.clone(), idx);
        self.chairs_by_token.insert(c.access_token.clone(), idx);
        if let Some(o) = c.owner {
            self.owners[o as usize].chairs.push(idx);
        }
        self.chairs.push(c);
        idx
    }

    // ------------------------------------------------------------ 通知イベント (push 時点のスナップショット)

    pub fn app_event(&self, ri: u32, status: St) -> Bytes {
        let r = &self.rides[ri as usize];
        let mut j = J::new();
        j.raw("{\"ride_id\":").str(&r.id);
        j.raw(",\"pickup_coordinate\":").coord(r.pickup_latitude, r.pickup_longitude);
        j.raw(",\"destination_coordinate\":").coord(r.destination_latitude, r.destination_longitude);
        j.raw(",\"fare\":").int(r.fare());
        j.raw(",\"status\":\"").raw(status.as_str()).raw("\"");
        if let Some(ci) = r.chair {
            let c = &self.chairs[ci as usize];
            j.raw(",\"chair\":{\"id\":").str(&c.id).raw(",\"name\":").str(&c.name).raw(",\"model\":").str(&c.model);
            j.raw(",\"stats\":{\"total_rides_count\":").int(c.completed_count).raw(",\"total_evaluation_avg\":");
            let avg = if c.completed_count > 0 { c.evaluation_sum as f64 / c.completed_count as f64 } else { 0.0 };
            j.float(avg).raw("}}");
        }
        j.raw(",\"created_at\":").int(us_to_ms(r.created_at));
        j.raw(",\"updated_at\":").int(us_to_ms(r.updated_at)).raw("}");
        Bytes::from(j.0)
    }

    pub fn chair_event(&self, ri: u32, status: St) -> Bytes {
        let r = &self.rides[ri as usize];
        let mut j = J::new();
        j.raw("{\"ride_id\":").str(&r.id);
        match r.user {
            Some(ui) => {
                let u = &self.users[ui as usize];
                let mut name = String::with_capacity(u.firstname.len() + u.lastname.len() + 1);
                name.push_str(&u.firstname);
                name.push(' ');
                name.push_str(&u.lastname);
                j.raw(",\"user\":{\"id\":").str(&u.id).raw(",\"name\":").str(&name).raw("}");
            }
            None => {
                j.raw(",\"user\":{\"id\":\"\",\"name\":\" \"}");
            }
        }
        j.raw(",\"pickup_coordinate\":").coord(r.pickup_latitude, r.pickup_longitude);
        j.raw(",\"destination_coordinate\":").coord(r.destination_latitude, r.destination_longitude);
        j.raw(",\"status\":\"").raw(status.as_str()).raw("\"}");
        Bytes::from(j.0)
    }

    /// ride に状態を追加して通知する
    pub fn add_ride_status(&mut self, ri: u32, status: St) {
        let t = self.now();
        let st = RideStatus { id: new_id(), status, created_at: t };
        persist_ride_status(&self.rides[ri as usize], &st);
        self.rides[ri as usize].statuses.push(st);
        let r = &self.rides[ri as usize];
        if let Some(ui) = r.user {
            let ev = self.app_event(ri, status);
            self.users[ui as usize].notifier.push(ev);
        }
        if let Some(ci) = r.chair {
            let ev = self.chair_event(ri, status);
            self.chairs[ci as usize].notifier.push(ev);
        }
    }

    // ------------------------------------------------------------ ロード

    /// DB から全データをロードする
    pub fn load_all(&mut self) -> Result<(), String> {
        self.reset_store();
        let mut conn = persist::db().get_conn().map_err(|e| e.to_string())?;
        let e = |e: mysql::Error| e.to_string();

        let rows: Vec<mysql::Row> = conn.query("SELECT name, value FROM settings").map_err(e)?;
        for r in rows {
            if vstr(&r, 0) == "payment_gateway_url" {
                self.payment_gateway_url = vstr(&r, 1);
            }
        }
        let rows: Vec<mysql::Row> = conn.query("SELECT name, speed FROM chair_models").map_err(e)?;
        for r in rows {
            self.chair_model_speed.insert(vstr(&r, 0), vint(&r, 1));
        }

        let rows: Vec<mysql::Row> =
            conn.query("SELECT id, name, access_token, chair_register_token, created_at, updated_at FROM owners ORDER BY id").map_err(e)?;
        for r in rows {
            self.add_owner(Owner {
                id: vstr(&r, 0),
                name: vstr(&r, 1),
                access_token: vstr(&r, 2),
                chair_register_token: vstr(&r, 3),
                created_at: vtime(&r, 4),
                updated_at: vtime(&r, 5),
                chairs: Vec::new(),
            });
        }

        let rows: Vec<mysql::Row> =
            conn.query("SELECT id, owner_id, name, model, is_active, access_token, created_at, updated_at FROM chairs ORDER BY id").map_err(e)?;
        for r in rows {
            self.add_chair(Chair {
                id: vstr(&r, 0),
                owner_id: vstr(&r, 1),
                name: vstr(&r, 2),
                model: vstr(&r, 3),
                is_active: vint(&r, 4) != 0,
                access_token: vstr(&r, 5),
                created_at: vtime(&r, 6),
                updated_at: vtime(&r, 7),
                owner: None,
                speed: 0,
                has_location: false,
                latitude: 0,
                longitude: 0,
                location_at: 0,
                total_distance: 0,
                total_distance_updated_at: 0,
                last_ride: None,
                busy: false,
                completed_count: 0,
                evaluation_sum: 0,
                completed: Vec::new(),
                nearby_granted: 0,
                nearby_grant_at: 0,
                notifier: Notifier::new(),
                free_pos: NOT_FREE,
            });
        }

        {
            let it = conn.query_iter("SELECT chair_id, latitude, longitude, created_at FROM chair_locations ORDER BY created_at, id").map_err(e)?;
            for r in it {
                let r = r.map_err(e)?;
                let Some(&ci) = self.chairs_by_id.get(&vstr(&r, 0)) else { continue };
                let (lat, lon, at) = (vint(&r, 1), vint(&r, 2), vtime(&r, 3));
                let c = &mut self.chairs[ci as usize];
                if c.has_location {
                    c.total_distance += calc_distance(lat, lon, c.latitude, c.longitude);
                }
                c.has_location = true;
                c.latitude = lat;
                c.longitude = lon;
                c.location_at = at;
                c.total_distance_updated_at = at;
            }
        }

        let rows: Vec<mysql::Row> = conn.query("SELECT chair_id, latitude, longitude, total_distance, location_at FROM chair_states").map_err(e)?;
        for r in rows {
            let Some(&ci) = self.chairs_by_id.get(&vstr(&r, 0)) else { continue };
            let c = &mut self.chairs[ci as usize];
            c.has_location = true;
            c.latitude = vint(&r, 1);
            c.longitude = vint(&r, 2);
            c.total_distance = vint(&r, 3);
            c.location_at = vtime(&r, 4);
            c.total_distance_updated_at = c.location_at;
        }

        let rows: Vec<mysql::Row> = conn
            .query("SELECT id, username, firstname, lastname, date_of_birth, access_token, invitation_code, created_at, updated_at FROM users")
            .map_err(e)?;
        for r in rows {
            self.add_user(User {
                id: vstr(&r, 0),
                username: vstr(&r, 1),
                firstname: vstr(&r, 2),
                lastname: vstr(&r, 3),
                date_of_birth: vstr(&r, 4),
                access_token: vstr(&r, 5),
                invitation_code: vstr(&r, 6),
                created_at: vtime(&r, 7),
                updated_at: vtime(&r, 8),
                payment_token: String::new(),
                coupons: Vec::new(),
                rides: Vec::new(),
                notifier: Notifier::new(),
            });
        }

        let rows: Vec<mysql::Row> = conn.query("SELECT user_id, token FROM payment_tokens").map_err(e)?;
        for r in rows {
            if let Some(&ui) = self.users_by_id.get(&vstr(&r, 0)) {
                self.users[ui as usize].payment_token = vstr(&r, 1);
            }
        }

        let rows: Vec<mysql::Row> = conn.query("SELECT user_id, code, discount, created_at, used_by FROM coupons ORDER BY created_at, code").map_err(e)?;
        let mut coupon_by_ride: FxHashMap<String, i64> = FxHashMap::default();
        for r in rows {
            let uid = vstr(&r, 0);
            let Some(&ui) = self.users_by_id.get(&uid) else { continue };
            let cp = Coupon { user_id: uid, code: vstr(&r, 1), discount: vint(&r, 2), created_at: vtime(&r, 3), used_by: vstr(&r, 4) };
            if !cp.used_by.is_empty() {
                coupon_by_ride.insert(cp.used_by.clone(), cp.discount);
            }
            if cp.code.len() > 4 && cp.code.as_bytes().starts_with(b"INV_") {
                *self.invitation_use_count.entry(cp.code[4..].to_string()).or_insert(0) += 1;
            }
            self.users[ui as usize].coupons.push(cp);
        }

        {
            let it = conn
                .query_iter("SELECT id, user_id, chair_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, evaluation, created_at, updated_at FROM rides ORDER BY created_at, id")
                .map_err(e)?;
            for r in it {
                let r = r.map_err(e)?;
                let id = vstr(&r, 0);
                let user_id = vstr(&r, 1);
                let chair_id = vstr(&r, 2);
                let ri = self.rides.len() as u32;
                let ride = Ride {
                    user: self.users_by_id.get(&user_id).copied(),
                    chair: self.chairs_by_id.get(&chair_id).copied(),
                    discount: coupon_by_ride.get(&id).copied().unwrap_or(0),
                    id,
                    user_id,
                    chair_id,
                    pickup_latitude: vint(&r, 3),
                    pickup_longitude: vint(&r, 4),
                    destination_latitude: vint(&r, 5),
                    destination_longitude: vint(&r, 6),
                    evaluation: vint(&r, 7),
                    created_at: vtime(&r, 8),
                    updated_at: vtime(&r, 9),
                    statuses: Vec::new(),
                    evaluating: false,
                    matched_at: 0,
                    eval_start_at: 0,
                };
                // 同じ id が複数あることはない (主キー) が、Go の map 同様に後勝ちにする
                if let Some(ui) = ride.user {
                    self.users[ui as usize].rides.push(ri);
                }
                self.rides_by_id.insert(ride.id.clone(), ri);
                self.rides.push(ride);
            }
        }

        {
            let it = conn.query_iter("SELECT id, ride_id, status, created_at FROM ride_statuses ORDER BY created_at, id").map_err(e)?;
            for r in it {
                let r = r.map_err(e)?;
                let Some(&ri) = self.rides_by_id.get(&vstr(&r, 1)) else { continue };
                let Some(status) = St::parse(&vstr(&r, 2)) else { continue };
                self.rides[ri as usize].statuses.push(RideStatus { id: vstr(&r, 0), status, created_at: vtime(&r, 3) });
            }
        }
        drop(conn);

        // 派生状態の再構築
        for ri in 0..self.rides.len() as u32 {
            let r = &mut self.rides[ri as usize];
            if r.statuses.is_empty() {
                r.statuses.push(RideStatus { id: new_id(), status: St::Matching, created_at: r.created_at });
            }
            let Some(ci) = r.chair else {
                if r.status() == St::Matching {
                    self.waiting_rides.push(ri);
                }
                continue;
            };
            let (upd, status, eval) = (r.updated_at, r.status(), r.evaluation);
            let last = self.chairs[ci as usize].last_ride;
            let replace = match last {
                None => true,
                Some(l) => upd > self.rides[l as usize].updated_at,
            };
            let c = &mut self.chairs[ci as usize];
            if replace {
                c.last_ride = Some(ri);
            }
            if status == St::Completed {
                c.completed_count += 1;
                c.evaluation_sum += eval;
                c.completed.push(ri);
            } else {
                c.busy = true;
            }
        }
        for ci in 0..self.chairs.len() as u32 {
            self.update_free(ci);
            let mut comp = std::mem::take(&mut self.chairs[ci as usize].completed);
            comp.sort_by_key(|&ri| self.rides[ri as usize].updated_at);
            self.chairs[ci as usize].completed = comp;
        }
        let mut waiting = std::mem::take(&mut self.waiting_rides);
        waiting.sort_by_key(|&ri| self.rides[ri as usize].created_at);
        self.waiting_rides = waiting;

        // 再起動直後の通知は最新状態のみを送る
        for ui in 0..self.users.len() {
            if let Some(&ri) = self.users[ui].rides.last() {
                let ev = self.app_event(ri, self.rides[ri as usize].status());
                self.users[ui].notifier.reset(ev);
            }
        }
        for ci in 0..self.chairs.len() {
            if let Some(ri) = self.chairs[ci].last_ride {
                let ev = self.chair_event(ri, self.rides[ri as usize].status());
                self.chairs[ci].notifier.reset(ev);
            }
        }
        if self.last_now <= wall_us() {
            self.last_now = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 行の値の取り出し (text / binary 両対応)

fn vstr(r: &mysql::Row, i: usize) -> String {
    match r.as_ref(i) {
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::Int(v)) => v.to_string(),
        Some(Value::UInt(v)) => v.to_string(),
        _ => String::new(),
    }
}

fn vint(r: &mysql::Row, i: usize) -> i64 {
    match r.as_ref(i) {
        Some(Value::Bytes(b)) => std::str::from_utf8(b).ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
        Some(Value::Int(v)) => *v,
        Some(Value::UInt(v)) => *v as i64,
        _ => 0,
    }
}

fn vtime(r: &mysql::Row, i: usize) -> i64 {
    match r.as_ref(i) {
        Some(Value::Bytes(b)) => std::str::from_utf8(b).ok().and_then(parse_datetime).unwrap_or(0),
        Some(Value::Date(y, mo, d, h, mi, s, us)) => {
            crate::util::datetime_to_us(*y as i64, *mo as i64, *d as i64, *h as i64, *mi as i64, *s as i64, *us as i64)
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------- 永続化 (persist.go の persistXxx)

pub fn persist_user(u: &User) {
    persist::enqueue(
        persist::T_USERS,
        u.id.clone(),
        vec![
            Val::Str(u.id.clone()),
            Val::Str(u.username.clone()),
            Val::Str(u.firstname.clone()),
            Val::Str(u.lastname.clone()),
            Val::Str(u.date_of_birth.clone()),
            Val::Str(u.access_token.clone()),
            Val::Str(u.invitation_code.clone()),
            Val::t_us(u.created_at),
            Val::t_us(u.updated_at),
        ],
    );
}

pub fn persist_owner(o: &Owner) {
    persist::enqueue(
        persist::T_OWNERS,
        o.id.clone(),
        vec![
            Val::Str(o.id.clone()),
            Val::Str(o.name.clone()),
            Val::Str(o.access_token.clone()),
            Val::Str(o.chair_register_token.clone()),
            Val::t_us(o.created_at),
            Val::t_us(o.updated_at),
        ],
    );
}

pub fn persist_chair(c: &Chair) {
    persist::enqueue(
        persist::T_CHAIRS,
        c.id.clone(),
        vec![
            Val::Str(c.id.clone()),
            Val::Str(c.owner_id.clone()),
            Val::Str(c.name.clone()),
            Val::Str(c.model.clone()),
            Val::Bool(c.is_active),
            Val::Str(c.access_token.clone()),
            Val::t_us(c.created_at),
            Val::t_us(c.updated_at),
        ],
    );
}

pub fn persist_ride(r: &Ride) {
    persist::enqueue(
        persist::T_RIDES,
        r.id.clone(),
        vec![
            Val::Str(r.id.clone()),
            Val::Str(r.user_id.clone()),
            Val::opt_str(&r.chair_id),
            Val::Int(r.pickup_latitude),
            Val::Int(r.pickup_longitude),
            Val::Int(r.destination_latitude),
            Val::Int(r.destination_longitude),
            if r.evaluation != 0 { Val::Int(r.evaluation) } else { Val::Nil },
            Val::t_us(r.created_at),
            Val::t_us(r.updated_at),
        ],
    );
}

pub fn persist_coupon(c: &Coupon) {
    let mut key = String::with_capacity(c.user_id.len() + 1 + c.code.len());
    key.push_str(&c.user_id);
    key.push('\0');
    key.push_str(&c.code);
    persist::enqueue(
        persist::T_COUPONS,
        key,
        vec![Val::Str(c.user_id.clone()), Val::Str(c.code.clone()), Val::Int(c.discount), Val::t_us(c.created_at), Val::opt_str(&c.used_by)],
    );
}

pub fn persist_payment_token(u: &User, t: i64) {
    persist::enqueue(persist::T_PAYMENT_TOKENS, u.id.clone(), vec![Val::Str(u.id.clone()), Val::Str(u.payment_token.clone()), Val::t_us(t)]);
}

pub fn persist_ride_status(r: &Ride, st: &RideStatus) {
    persist::enqueue(
        persist::T_RIDE_STATUSES,
        st.id.clone(),
        vec![
            Val::Str(st.id.clone()),
            Val::Str(r.id.clone()),
            Val::Str(st.status.as_str().to_string()),
            Val::t_us(st.created_at),
            Val::t_us(st.created_at),
            Val::t_us(st.created_at),
        ],
    );
}

/// 座標は履歴ではなく椅子ごとの集計 (最新位置・総移動距離・更新時刻) として保存する
/// 終了時に全ての通知ストリームを閉じる (graceful shutdown が SSE を待ち続けないように)
pub fn close_streams() {
    let st = STATE.lock();
    for u in &st.users {
        u.notifier.close();
    }
    for c in &st.chairs {
        c.notifier.close();
    }
}

pub fn persist_chair_location(c: &Chair, t: i64) {
    persist::enqueue(
        persist::T_CHAIR_STATES,
        c.id.clone(),
        vec![Val::Str(c.id.clone()), Val::Int(c.latitude), Val::Int(c.longitude), Val::Int(c.total_distance), Val::t_us(t)],
    );
}
