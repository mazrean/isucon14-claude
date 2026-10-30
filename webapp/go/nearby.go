package main

import (
	"net/http"
	"os"
	"strconv"
	"time"
)

// 空き椅子が無いときの nearby-chairs はロングポーリングにする。
// ユーザーは空き椅子が無いと毎 tick 履歴確認 + nearby を繰り返し、ベンチの CPU を空回りで消費する。
// 応答を保留し、近くの椅子が空いたら (空き椅子 1 台あたり nearbyPerChair 人まで) その時点で返す。

var nearbyCompletedGrace = envDuration("NEARBY_COMPLETED_GRACE", 400*time.Millisecond)

var (
	nearbyMaxHold    = envDuration("NEARBY_MAX_HOLD", 4*time.Second)
	nearbyPerChair   = envInt("NEARBY_PER_CHAIR", 2)
	nearbyMaxWaiting = envInt("NEARBY_MAX_WAITING", 1000)
	nearbyGrantTTL   = envDuration("NEARBY_GRANT_TTL", 500*time.Millisecond)
	nearbyWaiters    []*nearbyWaiter
)

type nearbyWaiter struct {
	lat, lon, dist int
	ch             chan []appGetNearbyChairsResponseChair
	done           bool // mu で保護
}

func envInt(key string, def int) int {
	if v, err := strconv.Atoi(os.Getenv(key)); err == nil {
		return v
	}
	return def
}

func nearbyVisible(c *Chair, recent time.Time) bool {
	if !c.IsActive || c.Busy || !c.HasLocation {
		return false
	}
	// 完了直後の椅子はベンチ側の状態反映が遅れるので含めない (完了 3s 以内は欠落が許容される)
	return c.LastRide == nil || !c.LastRide.UpdatedAt.After(recent)
}

// mu を保持して呼ぶ
func collectNearby(lat, lon, distance int) []appGetNearbyChairsResponseChair {
	nearby := []appGetNearbyChairsResponseChair{}
	recent := time.Now().Add(-nearbyCompletedGrace)
	for c := range freeChairs {
		if !nearbyVisible(c, recent) {
			continue
		}
		if calculateDistance(lat, lon, c.Latitude, c.Longitude) <= distance {
			nearby = append(nearby, appGetNearbyChairsResponseChair{
				ID: c.ID, Name: c.Name, Model: c.Model,
				CurrentCoordinate: Coordinate{Latitude: c.Latitude, Longitude: c.Longitude},
			})
		}
	}
	return nearby
}

// visible (nearbyVisible な椅子) のうち範囲内のものを返す。mu を保持して呼ぶ
func nearbyFrom(visible []*Chair, lat, lon, distance int) []appGetNearbyChairsResponseChair {
	nearby := []appGetNearbyChairsResponseChair{}
	for _, c := range visible {
		if calculateDistance(lat, lon, c.Latitude, c.Longitude) <= distance {
			nearby = append(nearby, appGetNearbyChairsResponseChair{
				ID: c.ID, Name: c.Name, Model: c.Model,
				CurrentCoordinate: Coordinate{Latitude: c.Latitude, Longitude: c.Longitude},
			})
		}
	}
	return nearby
}

func (wt *nearbyWaiter) wait(r *http.Request) []appGetNearbyChairsResponseChair {
	timer := time.NewTimer(nearbyMaxHold)
	defer timer.Stop()
	select {
	case res := <-wt.ch:
		return res
	case <-timer.C:
	case <-r.Context().Done():
	case <-shutdownCh:
	}
	mu.Lock()
	defer mu.Unlock()
	if wt.done {
		return <-wt.ch
	}
	wt.done = true
	return []appGetNearbyChairsResponseChair{}
}

// 空いた椅子を待っているユーザーに配る。mu を保持して呼ぶ
func dispatchNearby() {
	if len(nearbyWaiters) == 0 {
		return
	}
	recent := time.Now().Add(-nearbyCompletedGrace)
	now := time.Now()
	visible := make([]*Chair, 0, 32)
	slots := make([]*Chair, 0, 32)
	for c := range freeChairs {
		if c.NearbyGranted > 0 && now.Sub(c.NearbyGrantAt) > nearbyGrantTTL {
			// 起こしたユーザーのライドが別の椅子に割り当てられた場合など
			c.NearbyGranted = 0
		}
		if nearbyVisible(c, recent) {
			visible = append(visible, c)
			if c.NearbyGranted < nearbyPerChair {
				slots = append(slots, c)
			}
		}
	}
	if len(slots) == 0 {
		return
	}
	// 待ちライドが溢れると 30s 未マッチで失格になるので上限を設ける
	allowed := nearbyMaxWaiting - len(waitingRides)

	rest := nearbyWaiters[:0]
	for _, wt := range nearbyWaiters {
		if wt.done {
			continue
		}
		if allowed <= 0 {
			rest = append(rest, wt)
			continue
		}
		var best *Chair
		for _, c := range slots {
			if c.NearbyGranted < nearbyPerChair && calculateDistance(wt.lat, wt.lon, c.Latitude, c.Longitude) <= wt.dist {
				best = c
				break
			}
		}
		if best == nil {
			rest = append(rest, wt)
			continue
		}
		best.NearbyGranted++
		best.NearbyGrantAt = now
		allowed--
		wt.done = true
		wt.ch <- nearbyFrom(visible, wt.lat, wt.lon, wt.dist)
	}
	for i := len(rest); i < len(nearbyWaiters); i++ {
		nearbyWaiters[i] = nil
	}
	nearbyWaiters = rest
}
