package main

import (
	"errors"
	"net/http"
	"strconv"
	"time"
)

const (
	initialFare     = 500
	farePerDistance = 100
)

type ownerPostOwnersRequest struct {
	Name string `json:"name"`
}

type ownerPostOwnersResponse struct {
	ID                 string `json:"id"`
	ChairRegisterToken string `json:"chair_register_token"`
}

func ownerPostOwners(w http.ResponseWriter, r *http.Request) {
	req := &ownerPostOwnersRequest{}
	if err := bindJSON(r, req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if req.Name == "" {
		writeError(w, http.StatusBadRequest, errors.New("some of required fields(name) are empty"))
		return
	}

	mu.Lock()
	defer mu.Unlock()
	if _, ok := ownerNames[req.Name]; ok {
		writeError(w, http.StatusInternalServerError, errors.New("duplicate owner name"))
		return
	}
	t := now()
	o := &Owner{ID: newID(), Name: req.Name, AccessToken: secureRandomStr(32), ChairRegisterToken: secureRandomStr(32), CreatedAt: t, UpdatedAt: t}
	addOwner(o)
	persistOwner(o)

	http.SetCookie(w, &http.Cookie{Path: "/", Name: "owner_session", Value: o.AccessToken})
	writeJSON(w, http.StatusCreated, &ownerPostOwnersResponse{ID: o.ID, ChairRegisterToken: o.ChairRegisterToken})
}

type chairSales struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	Sales int    `json:"sales"`
}

type modelSales struct {
	Model string `json:"model"`
	Sales int    `json:"sales"`
}

type ownerGetSalesResponse struct {
	TotalSales int          `json:"total_sales"`
	Chairs     []chairSales `json:"chairs"`
	Models     []modelSales `json:"models"`
}

func ownerGetSales(w http.ResponseWriter, r *http.Request) {
	since := time.Unix(0, 0)
	until := time.Date(9999, 12, 31, 23, 59, 59, 0, time.UTC)
	if s := r.URL.Query().Get("since"); s != "" {
		parsed, err := strconv.ParseInt(s, 10, 64)
		if err != nil {
			writeError(w, http.StatusBadRequest, err)
			return
		}
		since = time.UnixMilli(parsed)
	}
	if s := r.URL.Query().Get("until"); s != "" {
		parsed, err := strconv.ParseInt(s, 10, 64)
		if err != nil {
			writeError(w, http.StatusBadRequest, err)
			return
		}
		until = time.UnixMilli(parsed)
	}
	until = until.Add(999 * time.Microsecond)
	owner := r.Context().Value(ctxOwner).(*Owner)

	mu.Lock()
	res := ownerGetSalesResponse{}
	modelSalesByModel := map[string]int{}
	modelOrder := []string{}
	for _, c := range owner.Chairs {
		sales := 0
		for _, ride := range c.Completed {
			if ride.UpdatedAt.Before(since) || ride.UpdatedAt.After(until) {
				continue
			}
			sales += ride.Sale()
		}
		res.TotalSales += sales
		res.Chairs = append(res.Chairs, chairSales{ID: c.ID, Name: c.Name, Sales: sales})
		if _, ok := modelSalesByModel[c.Model]; !ok {
			modelOrder = append(modelOrder, c.Model)
		}
		modelSalesByModel[c.Model] += sales
	}
	mu.Unlock()

	models := []modelSales{}
	for _, m := range modelOrder {
		models = append(models, modelSales{Model: m, Sales: modelSalesByModel[m]})
	}
	res.Models = models
	writeJSON(w, http.StatusOK, res)
}

type ownerGetChairResponse struct {
	Chairs []ownerGetChairResponseChair `json:"chairs"`
}

type ownerGetChairResponseChair struct {
	ID                     string `json:"id"`
	Name                   string `json:"name"`
	Model                  string `json:"model"`
	Active                 bool   `json:"active"`
	RegisteredAt           int64  `json:"registered_at"`
	TotalDistance          int    `json:"total_distance"`
	TotalDistanceUpdatedAt *int64 `json:"total_distance_updated_at,omitempty"`
}

func ownerGetChairs(w http.ResponseWriter, r *http.Request) {
	owner := r.Context().Value(ctxOwner).(*Owner)

	mu.Lock()
	res := ownerGetChairResponse{}
	for _, c := range owner.Chairs {
		ch := ownerGetChairResponseChair{
			ID: c.ID, Name: c.Name, Model: c.Model, Active: c.IsActive,
			RegisteredAt: c.CreatedAt.UnixMilli(), TotalDistance: c.TotalDistance,
		}
		if c.HasLocation {
			t := c.TotalDistanceUpdatedAt.UnixMilli()
			ch.TotalDistanceUpdatedAt = &t
		}
		res.Chairs = append(res.Chairs, ch)
	}
	mu.Unlock()
	writeJSON(w, http.StatusOK, res)
}
