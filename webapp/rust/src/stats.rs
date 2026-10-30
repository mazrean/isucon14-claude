// STATS=1 で 1 秒ごとに需給の状況をログに出す (stats.go と同じキー名)。
use crate::store::{LockTimer, STATE, St, State};
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

pub static CNT_LOCATIONS: AtomicI64 = AtomicI64::new(0);
pub static CNT_NEARBY: AtomicI64 = AtomicI64::new(0);
pub static CNT_NEARBY_EMPTY: AtomicI64 = AtomicI64::new(0);
pub static CNT_POST_RIDES: AtomicI64 = AtomicI64::new(0);
pub static CNT_SALES: AtomicI64 = AtomicI64::new(0);
pub static CNT_OWNER_CHAIRS: AtomicI64 = AtomicI64::new(0);
pub static CNT_POST_CHAIRS: AtomicI64 = AtomicI64::new(0);

/// ライドの区間ごとの所要時間 (ms) を累積する
pub fn record_timeline(st: &mut State, ri: u32) {
    let r = &st.rides[ri as usize];
    let at = |s: St| r.statuses.iter().rev().find(|x| x.status == s).map(|x| x.created_at).unwrap_or(0);
    let ms = |a: i64, b: i64| (b - a) as f64 / 1000.0;
    let d = [
        ms(r.created_at, r.matched_at),
        ms(r.matched_at, at(St::Enroute)),
        ms(at(St::Enroute), at(St::Pickup)),
        ms(at(St::Pickup), at(St::Carrying)),
        ms(at(St::Carrying), at(St::Arrived)),
        ms(at(St::Arrived), r.eval_start_at),
        ms(r.eval_start_at, at(St::Completed)),
    ];
    for (i, v) in d.iter().enumerate() {
        st.timeline_sum[i] += v;
    }
    st.timeline_n += 1;
}

pub fn stats_loop() {
    let mut next = Instant::now();
    loop {
        next += Duration::from_secs(1);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
        let mut st = STATE.lock();
        let ts = Instant::now();
        let (mut free, mut busy, mut inactive, mut speed_sum) = (0, 0, 0, 0i64);
        for c in &st.chairs {
            if !c.is_active {
                inactive += 1;
            } else if c.busy {
                busy += 1;
                speed_sum += c.speed;
            } else {
                free += 1;
            }
        }
        let waiting = st.waiting_rides.len();
        let oldest_ms = st.waiting_rides.first().map(|&ri| (crate::util::wall_us() - st.rides[ri as usize].created_at) / 1000).unwrap_or(0);
        let mut tl = [0i64; 7];
        if st.timeline_n > 0 {
            for (i, v) in tl.iter_mut().enumerate() {
                *v = (st.timeline_sum[i] / st.timeline_n as f64) as i64;
            }
        }
        let tl_n = st.timeline_n;
        let n_statuses: usize = st.rides.iter().map(|r| r.statuses.len()).sum();
        let (n_rides, n_users, n_owners, n_waiters) = (st.rides.len(), st.users.len(), st.owners.len(), st.nearby_waiters.len());
        st.timeline_sum = [0.0; 7];
        st.timeline_n = 0;
        st.lock_stat[2].add(ts.elapsed().as_micros() as i64);
        let ls = st.lock_stat;
        st.lock_stat = [LockTimer::default(); 3];
        drop(st);
        let pq = crate::persist::queue_len();
        let tl_s: Vec<String> = tl.iter().map(|v| v.to_string()).collect();
        crate::log_info!(
            "stats",
            "lock_match_max_ms={} lock_match_sum_ms={} lock_dispatch_max_ms={} lock_dispatch_sum_ms={} lock_stats_ms={} persist_queue={} rides={} statuses={} locations={} waiting={} oldest_ms={} free={} busy={} inactive={} busy_speed={} users={} nearby_waiters={} nearby={} nearby_empty={} post_rides={} sales={} owner_chairs={} post_chairs={} owners={} timeline_n={} timeline_ms=\"[{}]\"",
            ls[0].max_us / 1000,
            ls[0].sum_us / 1000,
            ls[1].max_us / 1000,
            ls[1].sum_us / 1000,
            ls[2].max_us / 1000,
            pq,
            n_rides,
            n_statuses,
            CNT_LOCATIONS.load(Relaxed),
            waiting,
            oldest_ms,
            free,
            busy,
            inactive,
            speed_sum,
            n_users,
            n_waiters,
            CNT_NEARBY.swap(0, Relaxed),
            CNT_NEARBY_EMPTY.swap(0, Relaxed),
            CNT_POST_RIDES.swap(0, Relaxed),
            CNT_SALES.load(Relaxed),
            CNT_OWNER_CHAIRS.load(Relaxed),
            CNT_POST_CHAIRS.load(Relaxed),
            n_owners,
            tl_n,
            tl_s.join(" ")
        );
    }
}
