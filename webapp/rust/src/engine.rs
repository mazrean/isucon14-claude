// マッチング (matching.go) と nearby-chairs のロングポーリング配布 (nearby.go)。
use crate::store::{St, STATE, State, calc_distance, persist_ride};
use crate::util::{J, env_duration, env_int, wall_us};
use parking_lot::{Condvar, Mutex};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const MATCHING_INTERVAL: Duration = Duration::from_millis(20);
/// これ未満の待ち時間のライドには、迎車距離 < 10*speed の椅子だけを割り当てる
static STRICT_WAIT_US: LazyLock<i64> =
    LazyLock::new(|| env_duration("MATCH_STRICT_WAIT", Duration::from_millis(3000)).as_micros() as i64);
/// これ以上待ったライドは ETA に関わらず最優先で割り当てる
static URGENT_WAIT_US: LazyLock<i64> =
    LazyLock::new(|| env_duration("MATCH_URGENT_WAIT", Duration::from_millis(3500)).as_micros() as i64);

pub struct NearbyCfg {
    pub completed_grace_us: i64,
    pub max_hold: Duration,
    pub per_chair: i64,
    pub max_waiting: i64,
    pub grant_ttl_us: i64,
}

pub static NEARBY: LazyLock<NearbyCfg> = LazyLock::new(|| NearbyCfg {
    completed_grace_us: env_duration("NEARBY_COMPLETED_GRACE", Duration::from_millis(400)).as_micros() as i64,
    max_hold: env_duration("NEARBY_MAX_HOLD", Duration::from_secs(4)),
    per_chair: env_int("NEARBY_PER_CHAIR", 2),
    max_waiting: env_int("NEARBY_MAX_WAITING", 1000),
    grant_ttl_us: env_duration("NEARBY_GRANT_TTL", Duration::from_millis(500)).as_micros() as i64,
});

static KICK: Mutex<bool> = Mutex::new(false);
static KICK_CV: Condvar = Condvar::new();

pub fn kick_matching() {
    let mut k = KICK.lock();
    if !*k {
        *k = true;
        KICK_CV.notify_one();
    }
}

pub fn matching_loop() {
    let mut next = Instant::now() + MATCHING_INTERVAL;
    loop {
        {
            let mut k = KICK.lock();
            while !*k {
                if KICK_CV.wait_until(&mut k, next).timed_out() {
                    break;
                }
            }
            *k = false;
        }
        let now = Instant::now();
        if now >= next {
            next += MATCHING_INTERVAL;
            if next <= now {
                next = now + MATCHING_INTERVAL;
            }
        }
        let mut st = STATE.lock();
        let t0 = Instant::now();
        run_matching(&mut st);
        let t1 = Instant::now();
        dispatch_nearby(&mut st);
        let t2 = Instant::now();
        st.lock_stat[0].add((t1 - t0).as_micros() as i64);
        st.lock_stat[1].add((t2 - t1).as_micros() as i64);
    }
}

#[derive(Clone, Copy)]
struct Pair {
    ride: u32,
    eta: i64,
    urgent: bool,
    created_at: i64,
}

#[inline]
fn better(a: &Pair, b: &Pair) -> bool {
    if a.urgent != b.urgent {
        return a.urgent;
    }
    if a.eta != b.eta {
        return a.eta < b.eta;
    }
    a.created_at < b.created_at
}

struct RideInfo {
    r: u32,
    plat: i64,
    plon: i64,
    created_at: i64,
    strict: bool,
    urgent: bool,
    taken: bool,
}

#[derive(Clone, Copy)]
struct Cand {
    best: Pair,
    idx: i64, // infos の index、-1 は候補なし
}

fn find_best(infos: &[RideInfo], lat: i64, lon: i64, speed: i64) -> Cand {
    let mut res = Cand { best: Pair { ride: 0, eta: 0, urgent: false, created_at: 0 }, idx: -1 };
    for (i, inf) in infos.iter().enumerate() {
        if inf.taken {
            continue;
        }
        let d = calc_distance(lat, lon, inf.plat, inf.plon);
        if inf.strict && d >= 10 * speed {
            continue;
        }
        let p = Pair { ride: inf.r, eta: (d + speed - 1) / speed, urgent: inf.urgent, created_at: inf.created_at };
        if res.idx < 0 || better(&p, &res.best) {
            res = Cand { best: p, idx: i as i64 };
        }
    }
    res
}

/// 椅子ごとの最良ライドを持って貪欲に確定する
fn run_matching(st: &mut State) {
    if st.waiting_rides.is_empty() || st.free_chairs.is_empty() {
        return;
    }
    let free: Vec<u32> = st.free_chairs.clone();
    let t = wall_us();
    let mut infos: Vec<RideInfo> = st
        .waiting_rides
        .iter()
        .map(|&ri| {
            let r = &st.rides[ri as usize];
            let waited = t - r.created_at;
            RideInfo {
                r: ri,
                plat: r.pickup_latitude,
                plon: r.pickup_longitude,
                created_at: r.created_at,
                strict: waited < *STRICT_WAIT_US,
                urgent: waited >= *URGENT_WAIT_US,
                taken: false,
            }
        })
        .collect();
    let chair_pos: Vec<(i64, i64, i64)> = free
        .iter()
        .map(|&ci| {
            let c = &st.chairs[ci as usize];
            (c.latitude, c.longitude, c.speed)
        })
        .collect();
    let mut cands: Vec<Cand> = chair_pos.iter().map(|&(la, lo, sp)| find_best(&infos, la, lo, sp)).collect();
    let mut matched = false;
    loop {
        let mut bi: i64 = -1;
        for (i, c) in cands.iter().enumerate() {
            if c.idx >= 0 && (bi < 0 || better(&c.best, &cands[bi as usize].best)) {
                bi = i as i64;
            }
        }
        if bi < 0 {
            break;
        }
        let b = cands[bi as usize];
        let taken = b.idx;
        assign(st, b.best.ride, free[bi as usize]);
        infos[taken as usize].taken = true;
        cands[bi as usize].idx = -1;
        matched = true;
        for i in 0..cands.len() {
            if cands[i].idx == taken {
                let (la, lo, sp) = chair_pos[i];
                cands[i] = find_best(&infos, la, lo, sp);
            }
        }
    }
    if !matched {
        return;
    }
    let rides = &st.rides;
    st.waiting_rides.retain(|&ri| rides[ri as usize].chair.is_none());
}

fn assign(st: &mut State, ri: u32, ci: u32) {
    let t = st.now();
    {
        let cid = st.chairs[ci as usize].id.clone();
        let r = &mut st.rides[ri as usize];
        r.chair = Some(ci);
        r.chair_id = cid;
        r.updated_at = t;
        r.matched_at = t;
    }
    {
        let c = &mut st.chairs[ci as usize];
        c.busy = true;
        c.nearby_granted = 0;
        c.last_ride = Some(ri);
    }
    st.update_free(ci);
    persist_ride(&st.rides[ri as usize]);
    let ev = st.chair_event(ri, St::Matching);
    st.chairs[ci as usize].notifier.push(ev, false);
}

// ---------------------------------------------------------------- nearby

#[inline]
fn nearby_visible(st: &State, ci: u32, recent: i64) -> bool {
    let c = &st.chairs[ci as usize];
    if !c.is_active || c.busy || !c.has_location {
        return false;
    }
    // 完了直後の椅子はベンチ側の状態反映が遅れるので含めない
    match c.last_ride {
        None => true,
        Some(ri) => st.rides[ri as usize].updated_at <= recent,
    }
}

fn chair_json(j: &mut J, st: &State, ci: u32) {
    let c = &st.chairs[ci as usize];
    j.raw("{\"id\":").str(&c.id).raw(",\"name\":").str(&c.name).raw(",\"model\":").str(&c.model);
    j.raw(",\"current_coordinate\":").coord(c.latitude, c.longitude).raw("}");
}

/// 範囲内の可視な空き椅子の JSON 配列
pub fn collect_nearby(st: &State, lat: i64, lon: i64, distance: i64) -> (Vec<u8>, usize) {
    let recent = wall_us() - NEARBY.completed_grace_us;
    let mut j = J::new();
    j.raw("[");
    let mut n = 0;
    for &ci in &st.free_chairs {
        if !nearby_visible(st, ci, recent) {
            continue;
        }
        let c = &st.chairs[ci as usize];
        if calc_distance(lat, lon, c.latitude, c.longitude) <= distance {
            if n > 0 {
                j.raw(",");
            }
            chair_json(&mut j, st, ci);
            n += 1;
        }
    }
    j.raw("]");
    (j.0, n)
}

/// 空いた椅子を待っているユーザーに配る
fn dispatch_nearby(st: &mut State) {
    if st.nearby_waiters.is_empty() {
        return;
    }
    let cfg = &*NEARBY;
    let now = wall_us();
    let recent = now - cfg.completed_grace_us;
    let mut visible: Vec<(u32, i64, i64)> = Vec::with_capacity(32);
    let mut slots: Vec<u32> = Vec::with_capacity(32);
    for k in 0..st.free_chairs.len() {
        let ci = st.free_chairs[k];
        {
            let c = &mut st.chairs[ci as usize];
            if c.nearby_granted > 0 && now - c.nearby_grant_at > cfg.grant_ttl_us {
                c.nearby_granted = 0;
            }
        }
        if nearby_visible(st, ci, recent) {
            let c = &st.chairs[ci as usize];
            visible.push((ci, c.latitude, c.longitude));
            if c.nearby_granted < cfg.per_chair {
                slots.push(ci);
            }
        }
    }
    if slots.is_empty() {
        return;
    }
    // 可視な椅子ごとの JSON 断片 (待ち人ごとに使い回す)
    let frags: Vec<Vec<u8>> = visible
        .iter()
        .map(|&(ci, _, _)| {
            let mut j = J::new();
            chair_json(&mut j, st, ci);
            j.0
        })
        .collect();
    let mut allowed = cfg.max_waiting - st.waiting_rides.len() as i64;
    let mut waiters = std::mem::take(&mut st.nearby_waiters);
    waiters.retain_mut(|wt| {
        let Some(tx) = wt.tx.as_ref() else { return false };
        if tx.is_closed() {
            return false; // 待ち側がタイムアウト / 切断済み (Go の done)
        }
        if allowed <= 0 {
            return true;
        }
        let mut best: Option<u32> = None;
        for &ci in &slots {
            let c = &st.chairs[ci as usize];
            if c.nearby_granted < cfg.per_chair && calc_distance(wt.lat, wt.lon, c.latitude, c.longitude) <= wt.dist {
                best = Some(ci);
                break;
            }
        }
        let Some(ci) = best else { return true };
        let c = &mut st.chairs[ci as usize];
        c.nearby_granted += 1;
        c.nearby_grant_at = now;
        allowed -= 1;
        let mut out = Vec::with_capacity(256);
        out.push(b'[');
        let mut n = 0;
        for (k, &(_, la, lo)) in visible.iter().enumerate() {
            if calc_distance(wt.lat, wt.lon, la, lo) <= wt.dist {
                if n > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&frags[k]);
                n += 1;
            }
        }
        out.push(b']');
        let _ = wt.tx.take().unwrap().send(out);
        false
    });
    st.nearby_waiters = waiters;
}
