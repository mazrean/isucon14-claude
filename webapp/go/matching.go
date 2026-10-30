package main

import (
	"time"
)

const (
	matchingInterval = 20 * time.Millisecond
	// これ未満の待ち時間のライドには、迎車距離 < 10*speed の椅子だけを割り当てる
	strictWait = 3000 * time.Millisecond
	// これ以上待ったライドは ETA に関わらず最優先で割り当てる (30s 未マッチは critical)
	urgentWait = 3500 * time.Millisecond
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
		t0 := time.Now()
		runMatching()
		t1 := time.Now()
		dispatchNearby()
		t2 := time.Now()
		lockStat[0].add(t1.Sub(t0))
		lockStat[1].add(t2.Sub(t1))
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
	free := make([]*Chair, 0, len(freeChairs))
	for c := range freeChairs {
		free = append(free, c)
	}
	if len(free) == 0 {
		return
	}

	// 全ペアをソートする代わりに、椅子ごとの最良ライドを持って貪欲に確定する
	// (結果は (urgent 優先, ETA 昇順, 古い順) で全ペアをソートした貪欲法と同じ)。
	// 空き椅子は少なく待ちライドは多いので、ロック保持時間が大幅に短くなる。
	t := time.Now()
	type rideInfo struct {
		r              *Ride
		strict, urgent bool
		taken          bool
	}
	infos := make([]rideInfo, len(waitingRides))
	for i, r := range waitingRides {
		waited := t.Sub(r.CreatedAt)
		infos[i] = rideInfo{r: r, strict: waited < strictWait, urgent: waited >= urgentWait}
	}
	better := func(a, b matchPair) bool {
		if a.urgent != b.urgent {
			return a.urgent
		}
		if a.eta != b.eta {
			return a.eta < b.eta
		}
		return a.ride.CreatedAt.Before(b.ride.CreatedAt)
	}
	type cand struct {
		best matchPair
		idx  int // infos の index、-1 は候補なし
	}
	findBest := func(c *Chair) cand {
		res := cand{idx: -1}
		for i := range infos {
			in := &infos[i]
			if in.taken {
				continue
			}
			r := in.r
			d := calculateDistance(c.Latitude, c.Longitude, r.PickupLatitude, r.PickupLongitude)
			if in.strict && d >= 10*c.Speed {
				continue
			}
			p := matchPair{ride: r, chair: c, eta: (d + c.Speed - 1) / c.Speed, dist: d, urgent: in.urgent}
			if res.idx < 0 || better(p, res.best) {
				res = cand{best: p, idx: i}
			}
		}
		return res
	}
	cands := make([]cand, len(free))
	for i, c := range free {
		cands[i] = findBest(c)
	}
	matched := false
	for {
		bi := -1
		for i := range cands {
			if cands[i].idx >= 0 && (bi < 0 || better(cands[i].best, cands[bi].best)) {
				bi = i
			}
		}
		if bi < 0 {
			break
		}
		taken := cands[bi].idx
		assign(cands[bi].best.ride, cands[bi].best.chair)
		infos[taken].taken = true
		cands[bi].idx = -1
		matched = true
		for i := range cands {
			if cands[i].idx == taken {
				cands[i] = findBest(free[i])
			}
		}
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
	r.MatchedAt = r.UpdatedAt
	c.Busy = true
	updateFree(c)
	c.NearbyGranted = 0
	c.LastRide = r
	persistRide(r)
	c.Notifier.push(chairEvent(r, "MATCHING"))
}
