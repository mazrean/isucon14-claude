package main

import (
	"errors"
	"net/http"
)

type chairPostChairsRequest struct {
	Name               string `json:"name"`
	Model              string `json:"model"`
	ChairRegisterToken string `json:"chair_register_token"`
}

type chairPostChairsResponse struct {
	ID      string `json:"id"`
	OwnerID string `json:"owner_id"`
}

func chairPostChairs(w http.ResponseWriter, r *http.Request) {
	req := &chairPostChairsRequest{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	if req.Name == "" || req.Model == "" || req.ChairRegisterToken == "" {
		writeError(w, http.StatusBadRequest, errors.New("some of required fields(name, model, chair_register_token) are empty"))
		return
	}

	mu.Lock()
	defer mu.Unlock()
	owner := ownersByRegister[req.ChairRegisterToken]
	if owner == nil {
		writeError(w, http.StatusUnauthorized, errors.New("invalid chair_register_token"))
		return
	}

	cntPostChairs.Add(1)
	t := now()
	c := &Chair{ID: newID(), OwnerID: owner.ID, Name: req.Name, Model: req.Model, AccessToken: secureRandomStr(32), CreatedAt: t, UpdatedAt: t}
	addChair(c)
	persistChair(c)

	http.SetCookie(w, &http.Cookie{Path: "/", Name: "chair_session", Value: c.AccessToken})
	writeJSON(w, http.StatusCreated, &chairPostChairsResponse{ID: c.ID, OwnerID: owner.ID})
}

type postChairActivityRequest struct {
	IsActive bool `json:"is_active"`
}

func chairPostActivity(w http.ResponseWriter, r *http.Request) {
	chair := r.Context().Value(ctxChair).(*Chair)
	req := &postChairActivityRequest{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	mu.Lock()
	if chair.IsActive != req.IsActive {
		chair.IsActive = req.IsActive
		updateFree(chair)
		chair.UpdatedAt = now()
		persistChair(chair)
		kickMatching()
	}
	mu.Unlock()

	w.WriteHeader(http.StatusNoContent)
}

type chairPostCoordinateResponse struct {
	RecordedAt int64 `json:"recorded_at"`
}

func chairPostCoordinate(w http.ResponseWriter, r *http.Request) {
	req := &Coordinate{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	chair := r.Context().Value(ctxChair).(*Chair)

	mu.Lock()
	t := now()
	if chair.HasLocation {
		chair.TotalDistance += abs(req.Latitude-chair.Latitude) + abs(req.Longitude-chair.Longitude)
	}
	first := !chair.HasLocation
	chair.HasLocation = true
	if first {
		updateFree(chair)
	}
	chair.Latitude, chair.Longitude = req.Latitude, req.Longitude
	chair.LocationAt = t
	chair.TotalDistanceUpdatedAt = t
	persistChairLocation(chair, t)
	cntLocations.Add(1)

	if ride := chair.LastRide; ride != nil {
		switch ride.Status() {
		case "ENROUTE":
			if req.Latitude == ride.PickupLatitude && req.Longitude == ride.PickupLongitude {
				addRideStatus(ride, "PICKUP")
			}
		case "CARRYING":
			if req.Latitude == ride.DestinationLatitude && req.Longitude == ride.DestinationLongitude {
				addRideStatus(ride, "ARRIVED")
			}
		}
	}
	if first {
		kickMatching()
	}
	mu.Unlock()

	writeJSON(w, http.StatusOK, &chairPostCoordinateResponse{RecordedAt: t.UnixMilli()})
}

type simpleUser struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

type chairGetNotificationResponseData struct {
	RideID                string     `json:"ride_id"`
	User                  simpleUser `json:"user"`
	PickupCoordinate      Coordinate `json:"pickup_coordinate"`
	DestinationCoordinate Coordinate `json:"destination_coordinate"`
	Status                string     `json:"status"`
}

type postChairRidesRideIDStatusRequest struct {
	Status string `json:"status"`
}

func chairPostRideStatus(w http.ResponseWriter, r *http.Request) {
	rideID := r.PathValue("ride_id")
	chair := r.Context().Value(ctxChair).(*Chair)
	req := &postChairRidesRideIDStatusRequest{}
	if err := bindJSON(r, req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}

	mu.Lock()
	defer mu.Unlock()
	ride := rides[rideID]
	if ride == nil {
		writeError(w, http.StatusNotFound, errors.New("ride not found"))
		return
	}
	if ride.ChairID != chair.ID {
		writeError(w, http.StatusBadRequest, errors.New("not assigned to this ride"))
		return
	}

	switch req.Status {
	case "ENROUTE":
		addRideStatus(ride, "ENROUTE")
	case "CARRYING":
		if ride.Status() != "PICKUP" {
			writeError(w, http.StatusBadRequest, errors.New("chair has not arrived yet"))
			return
		}
		addRideStatus(ride, "CARRYING")
	default:
		writeError(w, http.StatusBadRequest, errors.New("invalid status"))
		return
	}

	w.WriteHeader(http.StatusNoContent)
}
