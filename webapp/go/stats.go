package main

import (
	"log/slog"
	"os"
	"sync/atomic"
	"time"
)

var (
	cntLocations, cntNearby, cntNearbyEmpty, cntPostRides, cntSales, cntOwnerChairs, cntPostChairs atomic.Int64
)

// ライドの区間ごとの所要時間 (ms) の累積。mu で保護
// 0:match待ち 1:match→ENROUTE 2:ENROUTE→PICKUP 3:PICKUP→CARRYING 4:CARRYING→ARRIVED 5:ARRIVED→評価受信 6:評価受信→完了(決済)
var timelineSum [7]float64
var timelineN int

func recordTimeline(r *Ride) {
	at := map[string]time.Time{}
	for _, st := range r.Statuses {
		at[st.Status] = st.CreatedAt
	}
	ms := func(a, b time.Time) float64 { return float64(b.Sub(a).Microseconds()) / 1000 }
	d := [7]float64{
		ms(r.CreatedAt, r.MatchedAt), ms(r.MatchedAt, at["ENROUTE"]), ms(at["ENROUTE"], at["PICKUP"]),
		ms(at["PICKUP"], at["CARRYING"]), ms(at["CARRYING"], at["ARRIVED"]), ms(at["ARRIVED"], r.EvalStartAt), ms(r.EvalStartAt, at["COMPLETED"]),
	}
	for i := range d {
		timelineSum[i] += d[i]
	}
	timelineN++
}

// STATS=1 で 1 秒ごとに需給の状況をログに出す (シナリオ分析用)
func init() {
	if os.Getenv("STATS") == "" {
		return
	}
	go func() {
		for range time.Tick(time.Second) {
			mu.Lock()
			free, busy, inactive, speedSum := 0, 0, 0, 0
			for _, c := range chairList {
				switch {
				case !c.IsActive:
					inactive++
				case c.Busy:
					busy++
					speedSum += c.Speed
				default:
					free++
				}
			}
			waiting := len(waitingRides)
			oldest := time.Duration(0)
			if waiting > 0 {
				oldest = time.Since(waitingRides[0].CreatedAt)
			}
			nUsers := len(users)
			var tl [7]int
			if timelineN > 0 {
				for i := range tl {
					tl[i] = int(timelineSum[i] / float64(timelineN))
				}
			}
			tlN := timelineN
			nRides := len(rides)
			nStatuses := 0
			for _, r := range rides {
				nStatuses += len(r.Statuses)
			}
			timelineSum, timelineN = [7]float64{}, 0
			mu.Unlock()
			persistMu.Lock()
			pq := len(persistQueue)
			persistMu.Unlock()
			slog.Info("stats", "persist_queue", pq, "rides", nRides, "statuses", nStatuses, "locations", cntLocations.Load(), "waiting", waiting, "oldest_ms", oldest.Milliseconds(), "free", free, "busy", busy, "inactive", inactive, "busy_speed", speedSum, "users", nUsers, "nearby_waiters", len(nearbyWaiters),
				"nearby", cntNearby.Swap(0), "nearby_empty", cntNearbyEmpty.Swap(0), "post_rides", cntPostRides.Swap(0),
				"sales", cntSales.Load(), "owner_chairs", cntOwnerChairs.Load(), "post_chairs", cntPostChairs.Load(), "owners", len(owners), "timeline_n", tlN, "timeline_ms", tl)
		}
	}()
}
