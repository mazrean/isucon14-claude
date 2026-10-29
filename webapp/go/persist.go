package main

import (
	"log/slog"
	"strings"
	"sync"
	"time"
)

// write-behind 永続化。ミューテーションは行スナップショットの upsert として積み、
// 1 goroutine がまとめて MySQL に書く。

type tableDef struct {
	name   string
	cols   []string
	nKey   int // 先頭 nKey 列が主キー
	upsert bool
	sqlPre string
	sqlSuf string
}

func newTable(name string, nKey int, upsert bool, cols ...string) *tableDef {
	t := &tableDef{name: name, cols: cols, nKey: nKey, upsert: upsert}
	t.sqlPre = "INSERT INTO " + name + " (" + strings.Join(cols, ",") + ") VALUES "
	ups := make([]string, 0, len(cols))
	for _, c := range cols[nKey:] {
		ups = append(ups, c+"=VALUES("+c+")")
	}
	t.sqlSuf = " ON DUPLICATE KEY UPDATE " + strings.Join(ups, ",")
	return t
}

var (
	tUsers         = newTable("users", 1, false, "id", "username", "firstname", "lastname", "date_of_birth", "access_token", "invitation_code", "created_at", "updated_at")
	tOwners        = newTable("owners", 1, false, "id", "name", "access_token", "chair_register_token", "created_at", "updated_at")
	tChairs        = newTable("chairs", 1, true, "id", "owner_id", "name", "model", "is_active", "access_token", "created_at", "updated_at")
	tRides         = newTable("rides", 1, true, "id", "user_id", "chair_id", "pickup_latitude", "pickup_longitude", "destination_latitude", "destination_longitude", "evaluation", "created_at", "updated_at")
	tCoupons       = newTable("coupons", 2, true, "user_id", "code", "discount", "created_at", "used_by")
	tPaymentTokens = newTable("payment_tokens", 1, true, "user_id", "token", "created_at")
	tRideStatuses  = newTable("ride_statuses", 1, false, "id", "ride_id", "status", "created_at", "app_sent_at", "chair_sent_at")
	tChairLocs     = newTable("chair_locations", 1, false, "id", "chair_id", "latitude", "longitude", "created_at")
	tSettings      = newTable("settings", 1, true, "name", "value")

	tableOrder = []*tableDef{tSettings, tUsers, tOwners, tChairs, tRides, tCoupons, tPaymentTokens, tRideStatuses, tChairLocs}
)

type persistOp struct {
	t    *tableDef
	key  string
	vals []any
}

var (
	persistMu    sync.Mutex
	persistQueue []persistOp
	flushMu      sync.Mutex
	persistKick  = make(chan struct{}, 1)
)

func enqueue(t *tableDef, key string, vals ...any) {
	persistMu.Lock()
	persistQueue = append(persistQueue, persistOp{t: t, key: key, vals: vals})
	n := len(persistQueue)
	persistMu.Unlock()
	if n >= 2000 {
		select {
		case persistKick <- struct{}{}:
		default:
		}
	}
}

func dropPersistQueue() {
	persistMu.Lock()
	persistQueue = nil
	persistMu.Unlock()
}

func persistLoop() {
	ticker := time.NewTicker(200 * time.Millisecond)
	for {
		select {
		case <-ticker.C:
		case <-persistKick:
		}
		flushPersist()
	}
}

func flushPersist() {
	flushMu.Lock()
	defer flushMu.Unlock()
	persistMu.Lock()
	ops := persistQueue
	persistQueue = nil
	persistMu.Unlock()
	if len(ops) == 0 {
		return
	}

	byTable := map[*tableDef][]persistOp{}
	for _, op := range ops {
		byTable[op.t] = append(byTable[op.t], op)
	}
	for _, t := range tableOrder {
		rows := byTable[t]
		if len(rows) == 0 {
			continue
		}
		if t.upsert {
			// 同一キーは最後のスナップショットだけ残す
			last := make(map[string]int, len(rows))
			for i, op := range rows {
				last[op.key] = i
			}
			dedup := rows[:0:0]
			for i, op := range rows {
				if last[op.key] == i {
					dedup = append(dedup, op)
				}
			}
			rows = dedup
		}
		writeRows(t, rows)
	}
}

func writeRows(t *tableDef, rows []persistOp) {
	const chunk = 500
	ph := "(" + strings.TrimSuffix(strings.Repeat("?,", len(t.cols)), ",") + ")"
	for start := 0; start < len(rows); start += chunk {
		end := min(start+chunk, len(rows))
		var sb strings.Builder
		sb.WriteString(t.sqlPre)
		args := make([]any, 0, (end-start)*len(t.cols))
		for i := start; i < end; i++ {
			if i > start {
				sb.WriteByte(',')
			}
			sb.WriteString(ph)
			args = append(args, rows[i].vals...)
		}
		sb.WriteString(t.sqlSuf)
		for attempt := 0; ; attempt++ {
			_, err := db.Exec(sb.String(), args...)
			if err == nil {
				break
			}
			slog.Error("persist failed", "table", t.name, "err", err)
			if attempt >= 5 {
				break
			}
			time.Sleep(100 * time.Millisecond)
		}
	}
}

func nullStr(s string) any {
	if s == "" {
		return nil
	}
	return s
}

func persistUser(u *User) {
	enqueue(tUsers, u.ID, u.ID, u.Username, u.Firstname, u.Lastname, u.DateOfBirth, u.AccessToken, u.InvitationCode, u.CreatedAt, u.UpdatedAt)
}

func persistOwner(o *Owner) {
	enqueue(tOwners, o.ID, o.ID, o.Name, o.AccessToken, o.ChairRegisterToken, o.CreatedAt, o.UpdatedAt)
}

func persistChair(c *Chair) {
	enqueue(tChairs, c.ID, c.ID, c.OwnerID, c.Name, c.Model, c.IsActive, c.AccessToken, c.CreatedAt, c.UpdatedAt)
}

func persistRide(r *Ride) {
	var eval any
	if r.Evaluation != 0 {
		eval = r.Evaluation
	}
	enqueue(tRides, r.ID, r.ID, r.UserID, nullStr(r.ChairID), r.PickupLatitude, r.PickupLongitude, r.DestinationLatitude, r.DestinationLongitude, eval, r.CreatedAt, r.UpdatedAt)
}

func persistCoupon(c *Coupon) {
	enqueue(tCoupons, c.UserID+"\x00"+c.Code, c.UserID, c.Code, c.Discount, c.CreatedAt, nullStr(c.UsedBy))
}

func persistPaymentToken(u *User, t time.Time) {
	enqueue(tPaymentTokens, u.ID, u.ID, u.PaymentToken, t)
}

func persistRideStatus(r *Ride, st RideStatus) {
	enqueue(tRideStatuses, st.ID, st.ID, r.ID, st.Status, st.CreatedAt, st.CreatedAt, st.CreatedAt)
}

func persistChairLocation(c *Chair, id string, t time.Time) {
	enqueue(tChairLocs, id, id, c.ID, c.Latitude, c.Longitude, t)
}

func persistSetting(name, value string) {
	enqueue(tSettings, name, name, value)
}
