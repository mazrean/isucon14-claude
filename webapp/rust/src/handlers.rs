// ルーティングと各ハンドラ (main.go / app_handlers.go / chair_handlers.go / owner_handlers.go)。
use crate::engine::{NEARBY, collect_nearby, kick_matching};
use crate::http::{self, Resp, bare, err, json, json_cookie};
use crate::stats::*;
use crate::store::*;
use crate::util::{J, atoi, cookie_get, new_id, query_get, secure_random_str, us_to_ms, wall_ms, wall_us};
use crate::{SHUTDOWN, persist};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

/// json.NewDecoder(r.Body).Decode(v) 相当: 先頭の 1 値だけ読む。null は何もしない
fn decode<T: DeserializeOwned + Default>(b: &[u8]) -> Result<T, String> {
    let mut it = serde_json::Deserializer::from_slice(b).into_iter::<Option<T>>();
    match it.next() {
        None => Err("EOF".to_string()),
        Some(Ok(v)) => Ok(v.unwrap_or_default()),
        Some(Err(e)) => Err(if e.is_eof() { "unexpected EOF".to_string() } else { e.to_string() }),
    }
}

async fn read_body(b: Incoming) -> Bytes {
    match b.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => Bytes::new(),
    }
}

pub async fn handle(req: Request<Incoming>) -> Result<Resp, Infallible> {
    let (parts, body) = req.into_parts();
    let m = &parts.method;
    let path = parts.uri.path();
    let get = m == Method::GET || m == Method::HEAD;
    let post = m == Method::POST;
    let h = &parts.headers;
    let q = parts.uri.query();
    macro_rules! only {
        ($cond:expr, $allow:expr, $e:expr) => {
            if $cond { $e } else { http::method_not_allowed($allow) }
        };
    }
    let resp = match path {
        "/api/app/notification" => only!(get, "GET, HEAD", app_get_notification(h)),
        "/api/chair/notification" => only!(get, "GET, HEAD", chair_get_notification(h)),
        "/api/app/nearby-chairs" => only!(get, "GET, HEAD", app_get_nearby_chairs(h, q).await),
        "/api/chair/coordinate" => only!(post, "POST", chair_post_coordinate(h, &read_body(body).await)),
        "/api/app/rides" => {
            if get {
                app_get_rides(h)
            } else if post {
                app_post_rides(h, &read_body(body).await)
            } else {
                http::method_not_allowed("GET, HEAD, POST")
            }
        }
        "/api/app/rides/estimated-fare" => only!(post, "POST", app_post_rides_estimated_fare(h, &read_body(body).await)),
        "/api/app/users" => only!(post, "POST", app_post_users(&read_body(body).await)),
        "/api/app/payment-methods" => only!(post, "POST", app_post_payment_methods(h, &read_body(body).await)),
        "/api/owner/owners" => only!(post, "POST", owner_post_owners(&read_body(body).await)),
        "/api/owner/sales" => only!(get, "GET, HEAD", owner_get_sales(h, q)),
        "/api/owner/chairs" => only!(get, "GET, HEAD", owner_get_chairs(h)),
        "/api/chair/chairs" => only!(post, "POST", chair_post_chairs(&read_body(body).await)),
        "/api/chair/activity" => only!(post, "POST", chair_post_activity(h, &read_body(body).await)),
        "/api/internal/matching" => only!(get, "GET, HEAD", bare(204)),
        "/api/initialize" => only!(post, "POST", post_initialize(read_body(body).await).await),
        _ => {
            if let Some(id) = path_param(path, "/api/app/rides/", "/evaluation") {
                only!(post, "POST", app_post_ride_evaluation(h, id, read_body(body).await).await)
            } else if let Some(id) = path_param(path, "/api/chair/rides/", "/status") {
                only!(post, "POST", chair_post_ride_status(h, &id, &read_body(body).await))
            } else {
                http::not_found()
            }
        }
    };
    Ok(resp)
}

fn path_param(path: &str, pre: &str, suf: &str) -> Option<String> {
    let id = path.strip_prefix(pre)?.strip_suffix(suf)?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    crate::util::query_unescape(id, false)
}

// ---------------------------------------------------------------- 認証

fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Result<&'a str, Resp> {
    match cookie_get(h, name) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(err(401, &format!("{name} cookie is required"))),
    }
}

fn invalid_token() -> Resp {
    err(401, "invalid access token")
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

macro_rules! auth {
    ($st:expr, $map:ident, $tok:expr) => {
        match $st.$map.get($tok) {
            Some(&i) => i,
            None => return invalid_token(),
        }
    };
}

// ---------------------------------------------------------------- initialize

#[derive(Deserialize, Default)]
#[serde(default)]
struct InitReq {
    payment_server: Option<String>,
}

async fn post_initialize(body: Bytes) -> Resp {
    let req: InitReq = match decode(&body) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let pg = req.payment_server.unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let mut st = STATE.lock();
        persist::drop_queue();
        let res = {
            let mut w = persist::FLUSH.lock();
            let out = std::process::Command::new("../sql/init.sh").output();
            let (outs, mut r) = match out {
                Ok(o) => {
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                    let r = if o.status.success() { Ok(()) } else { Err(format!("{}", o.status)) };
                    (s, r)
                }
                Err(e) => (String::new(), Err(e.to_string())),
            };
            if r.is_ok() {
                r = persist::db_exec(&format!("UPDATE settings SET value = {} WHERE name = 'payment_gateway_url'", persist::sql_quote(&pg)));
            }
            if r.is_ok() {
                // データを初期化したので WAL も捨てる
                r = persist::wal_truncate(&mut w);
            }
            r.map_err(|e| format!("failed to initialize: {outs}: {e}"))
        };
        if let Err(e) = res {
            return err(500, &e);
        }
        if let Err(e) = st.load_all() {
            return err(500, &e);
        }
        st.payment_gateway_url = pg;
        json(200, b"{\"language\":\"rust\"}".to_vec())
    })
    .await
    .unwrap_or_else(|_| err(500, "initialize panicked"))
}

// ---------------------------------------------------------------- app

#[derive(Deserialize, Default)]
#[serde(default)]
struct PostUsersReq {
    username: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
    date_of_birth: Option<String>,
    invitation_code: Option<String>,
}

fn app_post_users(body: &[u8]) -> Resp {
    let req: PostUsersReq = match decode(body) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let username = req.username.unwrap_or_default();
    let firstname = req.firstname.unwrap_or_default();
    let lastname = req.lastname.unwrap_or_default();
    let dob = req.date_of_birth.unwrap_or_default();
    if username.is_empty() || firstname.is_empty() || lastname.is_empty() || dob.is_empty() {
        return err(400, "required fields(username, firstname, lastname, date_of_birth) are empty");
    }
    let user_id = new_id();
    let access_token = secure_random_str(32);
    let invitation_code = secure_random_str(15);

    let mut st = STATE.lock();
    let mut inviter: Option<u32> = None;
    let inv = req.invitation_code.unwrap_or_default();
    if !inv.is_empty() {
        if st.invitation_use_count.get(&inv).copied().unwrap_or(0) >= 3 {
            return err(400, "この招待コードは使用できません。");
        }
        match st.users_by_invitation.get(&inv) {
            Some(&i) => inviter = Some(i),
            None => return err(400, "この招待コードは使用できません。"),
        }
    }
    let t = st.now();
    let ui = st.add_user(User {
        id: user_id.clone(),
        username,
        firstname,
        lastname,
        date_of_birth: dob,
        access_token: access_token.clone(),
        invitation_code: invitation_code.clone(),
        created_at: t,
        updated_at: t,
        payment_token: String::new(),
        coupons: Vec::new(),
        rides: Vec::new(),
        notifier: Notifier::new(),
    });
    persist_user(&st.users[ui as usize]);

    // 初回登録キャンペーンのクーポンを付与
    let ct = st.now();
    let cp = Coupon { user_id: user_id.clone(), code: "CP_NEW2024".into(), discount: 3000, created_at: ct, used_by: String::new() };
    persist_coupon(&cp);
    st.users[ui as usize].coupons.push(cp);

    if let Some(inviter) = inviter {
        *st.invitation_use_count.entry(inv.clone()).or_insert(0) += 1;
        let ct = st.now();
        let cp = Coupon { user_id: user_id.clone(), code: format!("INV_{inv}"), discount: 1500, created_at: ct, used_by: String::new() };
        persist_coupon(&cp);
        st.users[ui as usize].coupons.push(cp);

        let rt = st.now();
        let rw = Coupon {
            user_id: st.users[inviter as usize].id.clone(),
            code: format!("RWD_{inv}_{}", us_to_ms(rt)),
            discount: 1000,
            created_at: rt,
            used_by: String::new(),
        };
        persist_coupon(&rw);
        st.users[inviter as usize].coupons.push(rw);
    }
    drop(st);

    let mut j = J::new();
    j.raw("{\"id\":").str(&user_id).raw(",\"invitation_code\":").str(&invitation_code).raw("}");
    json_cookie(201, j.0, "app_session", &access_token)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PaymentReq {
    token: Option<String>,
}

fn app_post_payment_methods(h: &HeaderMap, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    let req = decode::<PaymentReq>(body);
    let mut st = STATE.lock();
    let ui = auth!(st, users_by_token, tok);
    let req = match req {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let token = req.token.unwrap_or_default();
    if token.is_empty() {
        return err(400, "token is required but was empty");
    }
    if !st.users[ui as usize].payment_token.is_empty() {
        return err(500, "payment token already registered");
    }
    st.users[ui as usize].payment_token = token;
    let t = st.now();
    persist_payment_token(&st.users[ui as usize], t);
    bare(204)
}

fn app_get_rides(h: &HeaderMap) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    let st = STATE.lock();
    let ui = auth!(st, users_by_token, tok);
    let mut j = J::new();
    j.raw("{\"rides\":[");
    let mut n = 0;
    for &ri in st.users[ui as usize].rides.iter().rev() {
        let r = &st.rides[ri as usize];
        if r.status() != St::Completed {
            continue;
        }
        let Some(ci) = r.chair else { continue };
        let c = &st.chairs[ci as usize];
        let owner_name = c.owner.map(|o| st.owners[o as usize].name.as_str()).unwrap_or("");
        if n > 0 {
            j.raw(",");
        }
        n += 1;
        j.raw("{\"id\":").str(&r.id);
        j.raw(",\"pickup_coordinate\":").coord(r.pickup_latitude, r.pickup_longitude);
        j.raw(",\"destination_coordinate\":").coord(r.destination_latitude, r.destination_longitude);
        j.raw(",\"chair\":{\"id\":").str(&c.id).raw(",\"owner\":").str(owner_name).raw(",\"name\":").str(&c.name).raw(",\"model\":").str(&c.model).raw("}");
        j.raw(",\"fare\":").int(r.fare());
        j.raw(",\"evaluation\":").int(r.evaluation);
        j.raw(",\"requested_at\":").int(us_to_ms(r.created_at));
        j.raw(",\"completed_at\":").int(us_to_ms(r.updated_at)).raw("}");
    }
    drop(st);
    j.raw("]}");
    json(200, j.0)
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct Coord {
    latitude: Option<i64>,
    longitude: Option<i64>,
}

impl Coord {
    fn lat(&self) -> i64 {
        self.latitude.unwrap_or(0)
    }
    fn lon(&self) -> i64 {
        self.longitude.unwrap_or(0)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RidesReq {
    pickup_coordinate: Option<Coord>,
    destination_coordinate: Option<Coord>,
}

fn next_coupon(u: &User) -> Option<usize> {
    if let Some(i) = u.coupons.iter().position(|c| c.used_by.is_empty() && c.code == "CP_NEW2024") {
        return Some(i);
    }
    u.coupons.iter().position(|c| c.used_by.is_empty())
}

fn app_post_rides(h: &HeaderMap, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    let req = decode::<RidesReq>(body);
    let mut st = STATE.lock();
    let ui = auth!(st, users_by_token, tok);
    let req = match req {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let (Some(p), Some(d)) = (req.pickup_coordinate, req.destination_coordinate) else {
        return err(400, "required fields(pickup_coordinate, destination_coordinate) are empty");
    };
    for &ri in &st.users[ui as usize].rides {
        if st.rides[ri as usize].status() != St::Completed {
            return err(409, "ride already exists");
        }
    }
    let t = st.now();
    let ri = st.rides.len() as u32;
    let mut ride = Ride {
        id: new_id(),
        user_id: st.users[ui as usize].id.clone(),
        chair_id: String::new(),
        pickup_latitude: p.lat(),
        pickup_longitude: p.lon(),
        destination_latitude: d.lat(),
        destination_longitude: d.lon(),
        evaluation: 0,
        created_at: t,
        updated_at: t,
        user: Some(ui),
        chair: None,
        statuses: Vec::new(),
        discount: 0,
        evaluating: false,
        matched_at: 0,
        eval_start_at: 0,
    };
    if let Some(ci) = next_coupon(&st.users[ui as usize]) {
        let cp = &mut st.users[ui as usize].coupons[ci];
        cp.used_by = ride.id.clone();
        ride.discount = cp.discount;
        persist_coupon(cp);
    }
    CNT_POST_RIDES.fetch_add(1, Relaxed);
    st.users[ui as usize].rides.push(ri);
    st.rides_by_id.insert(ride.id.clone(), ri);
    persist_ride(&ride);
    let fare = ride.fare();
    let id = ride.id.clone();
    st.rides.push(ride);
    // 前のライドの通知ストリームに新しいライドのイベントを取らせない
    st.users[ui as usize].notifier.expire_streams();
    st.add_ride_status(ri, St::Matching);
    st.waiting_rides.push(ri);
    kick_matching();
    drop(st);

    let mut j = J::new();
    j.raw("{\"ride_id\":").str(&id).raw(",\"fare\":").int(fare).raw("}");
    json(202, j.0)
}

fn app_post_rides_estimated_fare(h: &HeaderMap, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    let req = decode::<RidesReq>(body);
    let discount = {
        let st = STATE.lock();
        let ui = auth!(st, users_by_token, tok);
        if req.is_err() {
            0
        } else {
            next_coupon(&st.users[ui as usize]).map(|i| st.users[ui as usize].coupons[i].discount).unwrap_or(0)
        }
    };
    let req = match req {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let (Some(p), Some(d)) = (req.pickup_coordinate, req.destination_coordinate) else {
        return err(400, "required fields(pickup_coordinate, destination_coordinate) are empty");
    };
    let metered = FARE_PER_DISTANCE.wrapping_mul(calc_distance(p.lat(), p.lon(), d.lat(), d.lon()));
    let discounted = INITIAL_FARE + (metered - discount).max(0);
    let mut j = J::new();
    j.raw("{\"fare\":").int(discounted).raw(",\"discount\":").int(INITIAL_FARE + metered - discounted).raw("}");
    json(200, j.0)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EvalReq {
    evaluation: Option<i64>,
}

async fn app_post_ride_evaluation(h: &HeaderMap, ride_id: String, body: Bytes) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    {
        let st = STATE.lock();
        let _ = auth!(st, users_by_token, tok);
    }
    let req: EvalReq = match decode(&body) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let evaluation = req.evaluation.unwrap_or(0);
    if !(1..=5).contains(&evaluation) {
        return err(400, "evaluation must be between 1 and 5");
    }
    let (ri, epoch, token, fare, pg_url, rid) = {
        let mut st = STATE.lock();
        let Some(&ri) = st.rides_by_id.get(&ride_id) else {
            return err(404, "ride not found");
        };
        let r = &st.rides[ri as usize];
        if r.status() != St::Arrived || r.evaluating {
            return err(400, "not arrived yet");
        }
        let token = r.user.map(|u| st.users[u as usize].payment_token.clone()).unwrap_or_default();
        if token.is_empty() {
            return err(400, "payment token not registered");
        }
        let fare = r.fare();
        let rid = r.id.clone();
        let r = &mut st.rides[ri as usize];
        r.evaluating = true;
        r.eval_start_at = wall_us();
        (ri, st.epoch, token, fare, st.payment_gateway_url.clone(), rid)
    };
    // クライアントが切断してもライドの状態を必ず確定させるため別タスクで実行する
    tokio::spawn(async move {
        let res = crate::payment::post_payment(&pg_url, &token, &rid, fare).await;
        let mut st = STATE.lock();
        if st.epoch != epoch {
            // initialize でストアが作り直された (Go では孤立したライドを更新するだけ)
            return match res {
                Ok(()) => {
                    let t = st.completion_time();
                    json(200, format!("{{\"completed_at\":{}}}", us_to_ms(t)).into_bytes())
                }
                Err(e) => err(502, &e),
            };
        }
        st.rides[ri as usize].evaluating = false;
        if let Err(e) = res {
            return err(502, &e);
        }
        let t = st.completion_time();
        let ci = {
            let r = &mut st.rides[ri as usize];
            r.evaluation = evaluation;
            r.updated_at = t;
            r.chair
        };
        persist_ride(&st.rides[ri as usize]);
        if let Some(ci) = ci {
            let c = &mut st.chairs[ci as usize];
            c.completed_count += 1;
            c.evaluation_sum += evaluation;
            c.completed.push(ri);
            c.busy = false;
            st.update_free(ci);
        }
        st.add_ride_status(ri, St::Completed);
        record_timeline(&mut st, ri);
        kick_matching();
        drop(st);
        json(200, format!("{{\"completed_at\":{}}}", us_to_ms(t)).into_bytes())
    })
    .await
    .unwrap_or_else(|_| err(500, "evaluation task failed"))
}

fn serve_sse(n: Arc<Notifier>) -> Resp {
    http::sse(n.subscribe())
}

fn app_get_notification(h: &HeaderMap) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    let n = {
        let st = STATE.lock();
        let ui = auth!(st, users_by_token, tok);
        st.users[ui as usize].notifier.clone()
    };
    serve_sse(n)
}

fn chair_get_notification(h: &HeaderMap) -> Resp {
    let tok = tri!(cookie(h, "chair_session"));
    let n = {
        let st = STATE.lock();
        let ci = auth!(st, chairs_by_token, tok);
        st.chairs[ci as usize].notifier.clone()
    };
    serve_sse(n)
}

async fn app_get_nearby_chairs(h: &HeaderMap, q: Option<&str>) -> Resp {
    let tok = tri!(cookie(h, "app_session"));
    {
        let st = STATE.lock();
        let _ = auth!(st, users_by_token, tok);
    }
    let lat_s = query_get(q, "latitude");
    let lon_s = query_get(q, "longitude");
    let dist_s = query_get(q, "distance");
    if lat_s.is_empty() || lon_s.is_empty() {
        return err(400, "latitude or longitude is empty");
    }
    let Some(lat) = atoi(lat_s.trim()) else { return err(400, "latitude is invalid") };
    let Some(lon) = atoi(lon_s.trim()) else { return err(400, "longitude is invalid") };
    let mut distance = 50;
    if !dist_s.is_empty() {
        match atoi(&dist_s) {
            Some(d) => distance = d,
            None => return err(400, "distance is invalid"),
        }
    }
    let (mut chairs, rx) = {
        let mut st = STATE.lock();
        let (arr, n) = collect_nearby(&st, lat, lon, distance);
        if n == 0 && !NEARBY.max_hold.is_zero() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            st.nearby_waiters.push(Waiter { lat, lon, dist: distance, tx: Some(tx) });
            (arr, Some(rx))
        } else {
            (arr, None)
        }
    };
    CNT_NEARBY.fetch_add(1, Relaxed);
    if let Some(mut rx) = rx {
        let mut shut = SHUTDOWN.subscribe();
        let got = tokio::select! {
            r = &mut rx => r.ok(),
            _ = tokio::time::sleep(NEARBY.max_hold) => None,
            _ = shut.wait_for(|v| *v) => None,
        };
        chairs = match got {
            Some(v) => v,
            None => {
                let _st = STATE.lock();
                match rx.try_recv() {
                    Ok(v) => v,
                    Err(_) => {
                        drop(rx); // dispatch から done に見えるようロック中に閉じる
                        b"[]".to_vec()
                    }
                }
            }
        };
        if chairs.len() <= 2 {
            CNT_NEARBY_EMPTY.fetch_add(1, Relaxed);
        }
    }
    let mut j = J(Vec::with_capacity(chairs.len() + 48));
    j.raw("{\"chairs\":").bytes(&chairs).raw(",\"retrieved_at\":").int(wall_ms()).raw("}");
    json(200, j.0)
}

// ---------------------------------------------------------------- owner

#[derive(Deserialize, Default)]
#[serde(default)]
struct OwnersReq {
    name: Option<String>,
}

fn owner_post_owners(body: &[u8]) -> Resp {
    let req: OwnersReq = match decode(body) {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let name = req.name.unwrap_or_default();
    if name.is_empty() {
        return err(400, "some of required fields(name) are empty");
    }
    let mut st = STATE.lock();
    if st.owner_names.contains(&name) {
        return err(500, "duplicate owner name");
    }
    let t = st.now();
    let o = Owner {
        id: new_id(),
        name,
        access_token: secure_random_str(32),
        chair_register_token: secure_random_str(32),
        created_at: t,
        updated_at: t,
        chairs: Vec::new(),
    };
    persist_owner(&o);
    let mut j = J::new();
    j.raw("{\"id\":").str(&o.id).raw(",\"chair_register_token\":").str(&o.chair_register_token).raw("}");
    let tok = o.access_token.clone();
    st.add_owner(o);
    drop(st);
    json_cookie(201, j.0, "owner_session", &tok)
}

fn parse_int_err(s: &str) -> String {
    let b = s.trim_start_matches(['+', '-']);
    let kind = if !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()) { "value out of range" } else { "invalid syntax" };
    format!("strconv.ParseInt: parsing {:?}: {kind}", s)
}

fn owner_get_sales(h: &HeaderMap, q: Option<&str>) -> Resp {
    let tok = tri!(cookie(h, "owner_session"));
    let st = STATE.lock();
    let oi = auth!(st, owners_by_token, tok);
    let mut since: i64 = 0;
    let mut until: i64 = 253_402_300_799_000_000; // 9999-12-31 23:59:59 UTC (µs)
    let s = query_get(q, "since");
    if !s.is_empty() {
        match atoi(&s) {
            Some(v) => since = v.saturating_mul(1000),
            None => return err(400, &parse_int_err(&s)),
        }
    }
    let s = query_get(q, "until");
    if !s.is_empty() {
        match atoi(&s) {
            Some(v) => until = v.saturating_mul(1000),
            None => return err(400, &parse_int_err(&s)),
        }
    }
    let until = until.saturating_add(999);
    CNT_SALES.fetch_add(1, Relaxed);
    let owner = &st.owners[oi as usize];
    let mut total = 0i64;
    let mut cj = J::new();
    let mut models: Vec<(&str, i64)> = Vec::new();
    if owner.chairs.is_empty() {
        cj.raw("null");
    } else {
        cj.raw("[");
    }
    for (k, &ci) in owner.chairs.iter().enumerate() {
        let c = &st.chairs[ci as usize];
        let mut sales = 0i64;
        for &ri in &c.completed {
            let r = &st.rides[ri as usize];
            if r.updated_at < since || r.updated_at > until {
                continue;
            }
            sales += r.sale();
        }
        total += sales;
        if k > 0 {
            cj.raw(",");
        }
        cj.raw("{\"id\":").str(&c.id).raw(",\"name\":").str(&c.name).raw(",\"sales\":").int(sales).raw("}");
        match models.iter_mut().find(|(m, _)| *m == c.model.as_str()) {
            Some(e) => e.1 += sales,
            None => models.push((c.model.as_str(), sales)),
        }
    }
    if !owner.chairs.is_empty() {
        cj.raw("]");
    }
    let mut j = J(Vec::with_capacity(cj.0.len() + 64 + models.len() * 48));
    j.raw("{\"total_sales\":").int(total).raw(",\"chairs\":").bytes(&cj.0).raw(",\"models\":[");
    for (k, (m, s)) in models.iter().enumerate() {
        if k > 0 {
            j.raw(",");
        }
        j.raw("{\"model\":").str(m).raw(",\"sales\":").int(*s).raw("}");
    }
    drop(st);
    j.raw("]}");
    json(200, j.0)
}

fn owner_get_chairs(h: &HeaderMap) -> Resp {
    let tok = tri!(cookie(h, "owner_session"));
    CNT_OWNER_CHAIRS.fetch_add(1, Relaxed);
    let st = STATE.lock();
    let oi = auth!(st, owners_by_token, tok);
    let owner = &st.owners[oi as usize];
    let mut j = J::new();
    if owner.chairs.is_empty() {
        j.raw("{\"chairs\":null}");
    } else {
        j.raw("{\"chairs\":[");
        for (k, &ci) in owner.chairs.iter().enumerate() {
            let c = &st.chairs[ci as usize];
            if k > 0 {
                j.raw(",");
            }
            j.raw("{\"id\":").str(&c.id).raw(",\"name\":").str(&c.name).raw(",\"model\":").str(&c.model);
            j.raw(",\"active\":").boolean(c.is_active).raw(",\"registered_at\":").int(us_to_ms(c.created_at));
            j.raw(",\"total_distance\":").int(c.total_distance);
            if c.has_location {
                j.raw(",\"total_distance_updated_at\":").int(us_to_ms(c.total_distance_updated_at));
            }
            j.raw("}");
        }
        j.raw("]}");
    }
    drop(st);
    json(200, j.0)
}

// ---------------------------------------------------------------- chair

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChairsReq {
    name: Option<String>,
    model: Option<String>,
    chair_register_token: Option<String>,
}

fn chair_post_chairs(body: &[u8]) -> Resp {
    let Ok(req) = decode::<ChairsReq>(body) else { return bare(400) };
    let name = req.name.unwrap_or_default();
    let model = req.model.unwrap_or_default();
    let reg = req.chair_register_token.unwrap_or_default();
    if name.is_empty() || model.is_empty() || reg.is_empty() {
        return err(400, "some of required fields(name, model, chair_register_token) are empty");
    }
    let mut st = STATE.lock();
    let Some(&oi) = st.owners_by_register.get(&reg) else {
        return err(401, "invalid chair_register_token");
    };
    CNT_POST_CHAIRS.fetch_add(1, Relaxed);
    let t = st.now();
    let owner_id = st.owners[oi as usize].id.clone();
    let c = Chair {
        id: new_id(),
        owner_id: owner_id.clone(),
        name,
        model,
        is_active: false,
        access_token: secure_random_str(32),
        created_at: t,
        updated_at: t,
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
    };
    let ci = st.add_chair(c);
    let c = &st.chairs[ci as usize];
    persist_chair(c);
    let mut j = J::new();
    j.raw("{\"id\":").str(&c.id).raw(",\"owner_id\":").str(&owner_id).raw("}");
    let tok = c.access_token.clone();
    drop(st);
    json_cookie(201, j.0, "chair_session", &tok)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ActivityReq {
    is_active: Option<bool>,
}

fn chair_post_activity(h: &HeaderMap, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "chair_session"));
    let req = decode::<ActivityReq>(body);
    let mut st = STATE.lock();
    let ci = auth!(st, chairs_by_token, tok);
    let Ok(req) = req else { return bare(400) };
    let active = req.is_active.unwrap_or(false);
    if st.chairs[ci as usize].is_active != active {
        st.chairs[ci as usize].is_active = active;
        st.update_free(ci);
        let t = st.now();
        st.chairs[ci as usize].updated_at = t;
        persist_chair(&st.chairs[ci as usize]);
        kick_matching();
    }
    bare(204)
}

fn chair_post_coordinate(h: &HeaderMap, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "chair_session"));
    let req = decode::<Coord>(body);
    let mut st = STATE.lock();
    let ci = auth!(st, chairs_by_token, tok);
    let Ok(req) = req else { return bare(400) };
    let (lat, lon) = (req.lat(), req.lon());
    let t = st.now();
    let first = {
        let c = &mut st.chairs[ci as usize];
        if c.has_location {
            c.total_distance += calc_distance(lat, lon, c.latitude, c.longitude);
        }
        let first = !c.has_location;
        c.has_location = true;
        first
    };
    if first {
        st.update_free(ci);
    }
    let last_ride = {
        let c = &mut st.chairs[ci as usize];
        c.latitude = lat;
        c.longitude = lon;
        c.location_at = t;
        c.total_distance_updated_at = t;
        persist_chair_location(c, t);
        c.last_ride
    };
    CNT_LOCATIONS.fetch_add(1, Relaxed);
    if let Some(ri) = last_ride {
        let r = &st.rides[ri as usize];
        match r.status() {
            St::Enroute if lat == r.pickup_latitude && lon == r.pickup_longitude => st.add_ride_status(ri, St::Pickup),
            St::Carrying if lat == r.destination_latitude && lon == r.destination_longitude => st.add_ride_status(ri, St::Arrived),
            _ => {}
        }
    }
    if first {
        kick_matching();
    }
    drop(st);
    let mut j = J(Vec::with_capacity(32));
    j.raw("{\"recorded_at\":").int(us_to_ms(t)).raw("}");
    json(200, j.0)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RideStatusReq {
    status: Option<String>,
}

fn chair_post_ride_status(h: &HeaderMap, ride_id: &str, body: &[u8]) -> Resp {
    let tok = tri!(cookie(h, "chair_session"));
    let req = decode::<RideStatusReq>(body);
    let mut st = STATE.lock();
    let ci = auth!(st, chairs_by_token, tok);
    let req = match req {
        Ok(r) => r,
        Err(e) => return err(400, &e),
    };
    let Some(&ri) = st.rides_by_id.get(ride_id) else {
        return err(404, "ride not found");
    };
    if st.rides[ri as usize].chair_id != st.chairs[ci as usize].id {
        return err(400, "not assigned to this ride");
    }
    match req.status.as_deref().unwrap_or("") {
        "ENROUTE" => st.add_ride_status(ri, St::Enroute),
        "CARRYING" => {
            if st.rides[ri as usize].status() != St::Pickup {
                return err(400, "chair has not arrived yet");
            }
            st.add_ride_status(ri, St::Carrying);
        }
        _ => return err(400, "invalid status"),
    }
    bare(204)
}
