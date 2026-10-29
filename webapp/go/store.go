package main

import (
	"database/sql"
	"sort"
	"sync"
	"time"
)

// 全データをメモリに持つ。ロックは単一の mu で守る。
var mu sync.Mutex

type User struct {
	ID             string
	Username       string
	Firstname      string
	Lastname       string
	DateOfBirth    string
	AccessToken    string
	InvitationCode string
	CreatedAt      time.Time
	UpdatedAt      time.Time

	PaymentToken string
	Coupons      []*Coupon // 付与順
	Rides        []*Ride   // 作成順
	Notifier     *Notifier
}

type Owner struct {
	ID                 string
	Name               string
	AccessToken        string
	ChairRegisterToken string
	CreatedAt          time.Time
	UpdatedAt          time.Time

	Chairs []*Chair // id 順
}

type Chair struct {
	ID          string
	OwnerID     string
	Name        string
	Model       string
	IsActive    bool
	AccessToken string
	CreatedAt   time.Time
	UpdatedAt   time.Time

	Owner *Owner
	Speed int

	HasLocation            bool
	Latitude               int
	Longitude              int
	LocationAt             time.Time
	TotalDistance          int
	TotalDistanceUpdatedAt time.Time

	LastRide       *Ride // 最後に割り当てられたライド
	Busy           bool  // 未完了のライドを持つ
	CompletedCount int
	EvaluationSum  int
	Completed      []*Ride // 完了したライド (完了順)

	Notifier *Notifier
}

type Coupon struct {
	UserID    string
	Code      string
	Discount  int
	CreatedAt time.Time
	UsedBy    string
}

type RideStatus struct {
	ID        string
	Status    string
	CreatedAt time.Time
}

type Ride struct {
	ID                   string
	UserID               string
	ChairID              string
	PickupLatitude       int
	PickupLongitude      int
	DestinationLatitude  int
	DestinationLongitude int
	Evaluation           int // 0 = 未評価
	CreatedAt            time.Time
	UpdatedAt            time.Time

	User       *User
	Chair      *Chair
	Statuses   []RideStatus
	Discount   int
	Evaluating bool
}

func (r *Ride) Status() string {
	return r.Statuses[len(r.Statuses)-1].Status
}

func (r *Ride) Fare() int {
	metered := farePerDistance * calculateDistance(r.PickupLatitude, r.PickupLongitude, r.DestinationLatitude, r.DestinationLongitude)
	return initialFare + max(metered-r.Discount, 0)
}

func (r *Ride) Sale() int {
	return calculateFare(r.PickupLatitude, r.PickupLongitude, r.DestinationLatitude, r.DestinationLongitude)
}

var (
	paymentGatewayURL string

	users              map[string]*User
	usersByToken       map[string]*User
	usersByInvitation  map[string]*User
	usernames          map[string]struct{}
	invitationUseCount map[string]int

	owners              map[string]*Owner
	ownersByToken       map[string]*Owner
	ownersByRegister    map[string]*Owner
	ownerNames          map[string]struct{}
	chairs              map[string]*Chair
	chairsByToken       map[string]*Chair
	chairList           []*Chair
	rides               map[string]*Ride
	waitingRides        []*Ride
	chairModelSpeed     map[string]int
)

func resetStore() {
	users = map[string]*User{}
	usersByToken = map[string]*User{}
	usersByInvitation = map[string]*User{}
	usernames = map[string]struct{}{}
	invitationUseCount = map[string]int{}
	owners = map[string]*Owner{}
	ownersByToken = map[string]*Owner{}
	ownersByRegister = map[string]*Owner{}
	ownerNames = map[string]struct{}{}
	chairs = map[string]*Chair{}
	chairsByToken = map[string]*Chair{}
	chairList = nil
	rides = map[string]*Ride{}
	waitingRides = nil
	chairModelSpeed = map[string]int{}
}

// 単調増加する時刻 (µs 精度)。同一時刻による並び順の曖昧さを無くす。
var lastNow time.Time

func now() time.Time {
	t := time.Now().Truncate(time.Microsecond)
	if !t.After(lastNow) {
		t = lastNow.Add(time.Microsecond)
	}
	lastNow = t
	return t
}

// 完了時刻はミリ秒単位で一意にする。owner/sales の until (ms) に
// 同一ミリ秒の別ライドが混ざってベンチの集計より大きくなるのを防ぐ。
var lastCompletedMs int64

func completionTime() time.Time {
	t := now()
	if ms := t.UnixMilli(); ms <= lastCompletedMs {
		t = time.UnixMilli(lastCompletedMs + 1)
		lastNow = t
	}
	lastCompletedMs = t.UnixMilli()
	return t
}

func addUser(u *User) {
	users[u.ID] = u
	usersByToken[u.AccessToken] = u
	usersByInvitation[u.InvitationCode] = u
	usernames[u.Username] = struct{}{}
	if u.Notifier == nil {
		u.Notifier = newNotifier()
	}
}

func addOwner(o *Owner) {
	owners[o.ID] = o
	ownersByToken[o.AccessToken] = o
	ownersByRegister[o.ChairRegisterToken] = o
	ownerNames[o.Name] = struct{}{}
}

func addChair(c *Chair) {
	chairs[c.ID] = c
	chairsByToken[c.AccessToken] = c
	chairList = append(chairList, c)
	c.Owner = owners[c.OwnerID]
	c.Speed = chairModelSpeed[c.Model]
	if c.Notifier == nil {
		c.Notifier = newNotifier()
	}
	if c.Owner != nil {
		c.Owner.Chairs = append(c.Owner.Chairs, c)
	}
}

// DB から全データをロードする。mu を保持した状態で呼ぶ。
func loadAll() error {
	resetStore()

	var settings []struct {
		Name  string `db:"name"`
		Value string `db:"value"`
	}
	if err := db.Select(&settings, "SELECT name, value FROM settings"); err != nil {
		return err
	}
	for _, s := range settings {
		if s.Name == "payment_gateway_url" {
			paymentGatewayURL = s.Value
		}
	}

	var models []struct {
		Name  string `db:"name"`
		Speed int    `db:"speed"`
	}
	if err := db.Select(&models, "SELECT name, speed FROM chair_models"); err != nil {
		return err
	}
	for _, m := range models {
		chairModelSpeed[m.Name] = m.Speed
	}

	var dbOwners []struct {
		ID                 string    `db:"id"`
		Name               string    `db:"name"`
		AccessToken        string    `db:"access_token"`
		ChairRegisterToken string    `db:"chair_register_token"`
		CreatedAt          time.Time `db:"created_at"`
		UpdatedAt          time.Time `db:"updated_at"`
	}
	if err := db.Select(&dbOwners, "SELECT * FROM owners ORDER BY id"); err != nil {
		return err
	}
	for _, o := range dbOwners {
		addOwner(&Owner{ID: o.ID, Name: o.Name, AccessToken: o.AccessToken, ChairRegisterToken: o.ChairRegisterToken, CreatedAt: o.CreatedAt, UpdatedAt: o.UpdatedAt})
	}

	var dbChairs []struct {
		ID          string    `db:"id"`
		OwnerID     string    `db:"owner_id"`
		Name        string    `db:"name"`
		Model       string    `db:"model"`
		IsActive    bool      `db:"is_active"`
		AccessToken string    `db:"access_token"`
		CreatedAt   time.Time `db:"created_at"`
		UpdatedAt   time.Time `db:"updated_at"`
	}
	if err := db.Select(&dbChairs, "SELECT * FROM chairs ORDER BY id"); err != nil {
		return err
	}
	for _, c := range dbChairs {
		addChair(&Chair{ID: c.ID, OwnerID: c.OwnerID, Name: c.Name, Model: c.Model, IsActive: c.IsActive, AccessToken: c.AccessToken, CreatedAt: c.CreatedAt, UpdatedAt: c.UpdatedAt})
	}

	var locs []struct {
		ChairID   string    `db:"chair_id"`
		Latitude  int       `db:"latitude"`
		Longitude int       `db:"longitude"`
		CreatedAt time.Time `db:"created_at"`
	}
	if err := db.Select(&locs, "SELECT chair_id, latitude, longitude, created_at FROM chair_locations ORDER BY created_at, id"); err != nil {
		return err
	}
	for _, l := range locs {
		c := chairs[l.ChairID]
		if c == nil {
			continue
		}
		if c.HasLocation {
			c.TotalDistance += abs(l.Latitude-c.Latitude) + abs(l.Longitude-c.Longitude)
		}
		c.HasLocation = true
		c.Latitude, c.Longitude = l.Latitude, l.Longitude
		c.LocationAt = l.CreatedAt
		c.TotalDistanceUpdatedAt = l.CreatedAt
	}

	var dbUsers []struct {
		ID             string    `db:"id"`
		Username       string    `db:"username"`
		Firstname      string    `db:"firstname"`
		Lastname       string    `db:"lastname"`
		DateOfBirth    string    `db:"date_of_birth"`
		AccessToken    string    `db:"access_token"`
		InvitationCode string    `db:"invitation_code"`
		CreatedAt      time.Time `db:"created_at"`
		UpdatedAt      time.Time `db:"updated_at"`
	}
	if err := db.Select(&dbUsers, "SELECT * FROM users"); err != nil {
		return err
	}
	for _, u := range dbUsers {
		addUser(&User{ID: u.ID, Username: u.Username, Firstname: u.Firstname, Lastname: u.Lastname, DateOfBirth: u.DateOfBirth, AccessToken: u.AccessToken, InvitationCode: u.InvitationCode, CreatedAt: u.CreatedAt, UpdatedAt: u.UpdatedAt})
	}

	var tokens []struct {
		UserID string `db:"user_id"`
		Token  string `db:"token"`
	}
	if err := db.Select(&tokens, "SELECT user_id, token FROM payment_tokens"); err != nil {
		return err
	}
	for _, t := range tokens {
		if u := users[t.UserID]; u != nil {
			u.PaymentToken = t.Token
		}
	}

	var dbCoupons []struct {
		UserID    string         `db:"user_id"`
		Code      string         `db:"code"`
		Discount  int            `db:"discount"`
		CreatedAt time.Time      `db:"created_at"`
		UsedBy    sql.NullString `db:"used_by"`
	}
	if err := db.Select(&dbCoupons, "SELECT * FROM coupons ORDER BY created_at, code"); err != nil {
		return err
	}
	couponByRide := map[string]*Coupon{}
	for _, c := range dbCoupons {
		u := users[c.UserID]
		if u == nil {
			continue
		}
		cp := &Coupon{UserID: c.UserID, Code: c.Code, Discount: c.Discount, CreatedAt: c.CreatedAt, UsedBy: c.UsedBy.String}
		u.Coupons = append(u.Coupons, cp)
		if cp.UsedBy != "" {
			couponByRide[cp.UsedBy] = cp
		}
		if len(cp.Code) > 4 && cp.Code[:4] == "INV_" {
			invitationUseCount[cp.Code[4:]]++
		}
	}

	var dbRides []struct {
		ID                   string         `db:"id"`
		UserID               string         `db:"user_id"`
		ChairID              sql.NullString `db:"chair_id"`
		PickupLatitude       int            `db:"pickup_latitude"`
		PickupLongitude      int            `db:"pickup_longitude"`
		DestinationLatitude  int            `db:"destination_latitude"`
		DestinationLongitude int            `db:"destination_longitude"`
		Evaluation           sql.NullInt64  `db:"evaluation"`
		CreatedAt            time.Time      `db:"created_at"`
		UpdatedAt            time.Time      `db:"updated_at"`
	}
	if err := db.Select(&dbRides, "SELECT * FROM rides ORDER BY created_at, id"); err != nil {
		return err
	}
	for _, r := range dbRides {
		ride := &Ride{ID: r.ID, UserID: r.UserID, ChairID: r.ChairID.String, PickupLatitude: r.PickupLatitude, PickupLongitude: r.PickupLongitude, DestinationLatitude: r.DestinationLatitude, DestinationLongitude: r.DestinationLongitude, Evaluation: int(r.Evaluation.Int64), CreatedAt: r.CreatedAt, UpdatedAt: r.UpdatedAt}
		ride.User = users[r.UserID]
		ride.Chair = chairs[ride.ChairID]
		if cp := couponByRide[r.ID]; cp != nil {
			ride.Discount = cp.Discount
		}
		rides[ride.ID] = ride
		if ride.User != nil {
			ride.User.Rides = append(ride.User.Rides, ride)
		}
	}

	var statuses []struct {
		ID        string    `db:"id"`
		RideID    string    `db:"ride_id"`
		Status    string    `db:"status"`
		CreatedAt time.Time `db:"created_at"`
	}
	if err := db.Select(&statuses, "SELECT id, ride_id, status, created_at FROM ride_statuses ORDER BY created_at, id"); err != nil {
		return err
	}
	for _, s := range statuses {
		if r := rides[s.RideID]; r != nil {
			r.Statuses = append(r.Statuses, RideStatus{ID: s.ID, Status: s.Status, CreatedAt: s.CreatedAt})
		}
	}

	// 派生状態の再構築
	for _, r := range rides {
		if len(r.Statuses) == 0 {
			r.Statuses = []RideStatus{{ID: newID(), Status: "MATCHING", CreatedAt: r.CreatedAt}}
		}
		if r.Chair == nil {
			if r.Status() == "MATCHING" {
				waitingRides = append(waitingRides, r)
			}
			continue
		}
		c := r.Chair
		if c.LastRide == nil || r.UpdatedAt.After(c.LastRide.UpdatedAt) {
			c.LastRide = r
		}
		if r.Status() == "COMPLETED" {
			c.CompletedCount++
			c.EvaluationSum += r.Evaluation
			c.Completed = append(c.Completed, r)
		} else {
			c.Busy = true
		}
	}
	for _, c := range chairList {
		sort.Slice(c.Completed, func(i, j int) bool { return c.Completed[i].UpdatedAt.Before(c.Completed[j].UpdatedAt) })
	}
	sort.Slice(waitingRides, func(i, j int) bool { return waitingRides[i].CreatedAt.Before(waitingRides[j].CreatedAt) })

	// 再起動直後の通知は最新状態のみを送る
	for _, u := range users {
		if len(u.Rides) > 0 {
			r := u.Rides[len(u.Rides)-1]
			u.Notifier.reset(appEvent(r, r.Status()))
		}
	}
	for _, c := range chairList {
		if c.LastRide != nil {
			c.Notifier.reset(chairEvent(c.LastRide, c.LastRide.Status()))
		}
	}
	if !lastNow.After(time.Now()) {
		lastNow = time.Time{}
	}
	return nil
}
