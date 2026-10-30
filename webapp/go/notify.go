package main

import (
	"encoding/json"
	"net/http"
	"time"
)

type notifyEvent struct {
	data []byte // 送信する JSON (push 時点のスナップショット)
}

func appEvent(r *Ride, status string) notifyEvent {
	b, _ := json.Marshal(appNotificationData(r, status))
	return notifyEvent{data: b}
}

func chairEvent(r *Ride, status string) notifyEvent {
	b, _ := json.Marshal(chairNotificationData(r, status))
	return notifyEvent{data: b}
}

// ユーザー / 椅子ごとの通知キュー。mu で保護される。
type Notifier struct {
	queue []notifyEvent
	last  *notifyEvent
	ch    chan struct{}
	gen   int // 最新の接続の世代。古い接続は自分の世代でなくなったら終了する
}

func newNotifier() *Notifier {
	return &Notifier{ch: make(chan struct{}, 1)}
}

func (n *Notifier) push(ev notifyEvent) {
	n.queue = append(n.queue, ev)
	select {
	case n.ch <- struct{}{}:
	default:
	}
}

// 既存の接続を全て古い世代にする (新しいライドの作成時に呼ぶ)。
// ベンチはライドごとに通知ストリームを張り直すので、作成時点で生きている接続は前のライドのもの。
// それが新しいライドの ENROUTE を取り出して閉じた接続に書くと、ベンチは ENROUTE を受け取れず、
// 前のライドの検証済みイベントと比較して「配車位置が一致しません」になる。
func (n *Notifier) expireStreams() {
	n.gen++
	select {
	case n.ch <- struct{}{}:
	default:
	}
}

func (n *Notifier) reset(ev notifyEvent) {
	n.queue = nil
	n.last = &ev
}

// 送るべきイベントを取り出す。first なら未送信が無くても最新を返す。
func (n *Notifier) take(first bool) []notifyEvent {
	if len(n.queue) == 0 {
		if first && n.last != nil {
			return []notifyEvent{*n.last}
		}
		return nil
	}
	evs := n.queue
	n.queue = nil
	n.last = &evs[len(evs)-1]
	return evs
}

// ride に状態を追加して通知する。mu を保持して呼ぶ。
func addRideStatus(r *Ride, status string) {
	t := now()
	st := RideStatus{ID: newID(), Status: status, CreatedAt: t}
	r.Statuses = append(r.Statuses, st)
	persistRideStatus(r, st)
	r.User.Notifier.push(appEvent(r, status))
	if r.Chair != nil {
		r.Chair.Notifier.push(chairEvent(r, status))
	}
}

func appNotificationData(r *Ride, status string) *appGetNotificationResponseData {
	d := &appGetNotificationResponseData{
		RideID:                r.ID,
		PickupCoordinate:      Coordinate{Latitude: r.PickupLatitude, Longitude: r.PickupLongitude},
		DestinationCoordinate: Coordinate{Latitude: r.DestinationLatitude, Longitude: r.DestinationLongitude},
		Fare:                  r.Fare(),
		Status:                status,
		CreatedAt:             r.CreatedAt.UnixMilli(),
		UpdateAt:              r.UpdatedAt.UnixMilli(),
	}
	if c := r.Chair; c != nil {
		stats := appGetNotificationResponseChairStats{TotalRidesCount: c.CompletedCount}
		if c.CompletedCount > 0 {
			stats.TotalEvaluationAvg = float64(c.EvaluationSum) / float64(c.CompletedCount)
		}
		d.Chair = &appGetNotificationResponseChair{ID: c.ID, Name: c.Name, Model: c.Model, Stats: stats}
	}
	return d
}

func chairNotificationData(r *Ride, status string) *chairGetNotificationResponseData {
	u := r.User
	return &chairGetNotificationResponseData{
		RideID:                r.ID,
		User:                  simpleUser{ID: u.ID, Name: u.Firstname + " " + u.Lastname},
		PickupCoordinate:      Coordinate{Latitude: r.PickupLatitude, Longitude: r.PickupLongitude},
		DestinationCoordinate: Coordinate{Latitude: r.DestinationLatitude, Longitude: r.DestinationLongitude},
		Status:                status,
	}
}

func serveSSE(w http.ResponseWriter, r *http.Request, n *Notifier) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		writeError(w, http.StatusInternalServerError, errNoFlusher)
		return
	}
	h := w.Header()
	h.Set("Content-Type", "text/event-stream")
	h.Set("Cache-Control", "no-cache")
	h.Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	ctx := r.Context()
	mu.Lock()
	n.gen++
	gen := n.gen
	mu.Unlock()
	// 古い接続を起こして終了させる
	select {
	case n.ch <- struct{}{}:
	default:
	}
	first := true
	buf := make([]byte, 0, 1024)
	for {
		mu.Lock()
		if n.gen != gen {
			mu.Unlock()
			select {
			case n.ch <- struct{}{}:
			default:
			}
			return
		}
		evs := n.take(first)
		buf = buf[:0]
		for _, ev := range evs {
			buf = append(buf, "data: "...)
			buf = append(buf, ev.data...)
			buf = append(buf, '\n', '\n')
		}
		mu.Unlock()
		first = false
		if len(buf) > 0 {
			if _, err := w.Write(buf); err != nil {
				return
			}
			flusher.Flush()
		}
		select {
		case <-ctx.Done():
			return
		case <-shutdownCh:
			return
		case <-n.ch:
		case <-time.After(30 * time.Second):
		}
	}
}

func appGetNotification(w http.ResponseWriter, r *http.Request) {
	user := r.Context().Value(ctxUser).(*User)
	serveSSE(w, r, user.Notifier)
}

func chairGetNotification(w http.ResponseWriter, r *http.Request) {
	chair := r.Context().Value(ctxChair).(*Chair)
	serveSSE(w, r, chair.Notifier)
}
