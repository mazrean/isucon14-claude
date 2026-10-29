package main

import (
	"sort"
	"time"
)

const (
	matchingInterval = 20 * time.Millisecond
	// これ未満の待ち時間のライドには、迎車距離 < 10*speed の椅子だけを割り当てる
	strictWait = 2000 * time.Millisecond
	// これ以上待ったライドは ETA に関わらず最優先で割り当てる (30s 未マッチは critical)
	urgentWait = 2500 * time.Millisecond
)

var matchingKick = make(chan struct{}, 1)

func kickMatching() {
	select {
	case matchingKick <- struct{}{}:
	default:
	}
}

func matchingLoop() {
	ticker := time.NewTicker(matchingInterval)
	for {
		select {
		case <-ticker.C:
		case <-matchingKick:
		}
		mu.Lock()
		runMatching()
		mu.Unlock()
	}
}

type matchPair struct {
	ride   *Ride
	chair  *Chair
	eta    int
	dist   int
	urgent bool
}

// mu を保持して呼ぶ
func runMatching() {
	if len(waitingRides) == 0 {
		return
	}
	free := make([]*Chair, 0, 64)
	for _, c := range chairList {
		if c.IsActive && !c.Busy && c.HasLocation && c.Speed > 0 {
			free = append(free, c)
		}
	}
	if len(free) == 0 {
		return
	}

	t := time.Now()
	pairs := make([]matchPair, 0, len(free)*len(waitingRides))
	for _, r := range waitingRides {
		waited := t.Sub(r.CreatedAt)
		strict := waited < strictWait
		urgent := waited >= urgentWait
		for _, c := range free {
			d := calculateDistance(c.Latitude, c.Longitude, r.PickupLatitude, r.PickupLongitude)
			if strict && d >= 10*c.Speed {
				continue
			}
			pairs = append(pairs, matchPair{ride: r, chair: c, eta: (d + c.Speed - 1) / c.Speed, dist: d, urgent: urgent})
		}
	}
	sort.Slice(pairs, func(i, j int) bool {
		if pairs[i].urgent != pairs[j].urgent {
			return pairs[i].urgent
		}
		if pairs[i].eta != pairs[j].eta {
			return pairs[i].eta < pairs[j].eta
		}
		return pairs[i].ride.CreatedAt.Before(pairs[j].ride.CreatedAt)
	})

	matched := false
	for _, p := range pairs {
		if p.ride.Chair != nil || p.chair.Busy {
			continue
		}
		assign(p.ride, p.chair)
		matched = true
	}
	if !matched {
		return
	}
	rest := waitingRides[:0]
	for _, r := range waitingRides {
		if r.Chair == nil {
			rest = append(rest, r)
		}
	}
	for i := len(rest); i < len(waitingRides); i++ {
		waitingRides[i] = nil
	}
	waitingRides = rest
}

func assign(r *Ride, c *Chair) {
	r.Chair = c
	r.ChairID = c.ID
	r.UpdatedAt = now()
	c.Busy = true
	c.LastRide = r
	persistRide(r)
	c.Notifier.push(chairEvent(r, "MATCHING"))
}
