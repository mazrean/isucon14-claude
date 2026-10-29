package main

import (
	"log/slog"
	"os"
	"sync/atomic"
	"time"
)

var (
	cntNearby, cntNearbyEmpty, cntPostRides, cntSales, cntOwnerChairs, cntPostChairs atomic.Int64
)

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
			mu.Unlock()
			slog.Info("stats", "waiting", waiting, "oldest_ms", oldest.Milliseconds(), "free", free, "busy", busy, "inactive", inactive, "busy_speed", speedSum, "users", nUsers,
				"nearby", cntNearby.Swap(0), "nearby_empty", cntNearbyEmpty.Swap(0), "post_rides", cntPostRides.Swap(0),
				"sales", cntSales.Load(), "owner_chairs", cntOwnerChairs.Load(), "post_chairs", cntPostChairs.Load(), "owners", len(owners))
		}
	}()
}
