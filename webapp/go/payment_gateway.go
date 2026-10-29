package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"time"
)

type paymentGatewayPostPaymentRequest struct {
	Amount int `json:"amount"`
}

var paymentClient = &http.Client{
	Timeout: 5 * time.Second,
	Transport: &http.Transport{
		DialContext:         (&net.Dialer{Timeout: 2 * time.Second, KeepAlive: 30 * time.Second}).DialContext,
		MaxIdleConns:        512,
		MaxIdleConnsPerHost: 512,
		IdleConnTimeout:     90 * time.Second,
	},
}

// Idempotency-Key (ride ID) 付きで成功するまでリトライする
func requestPaymentGatewayPostPayment(ctx context.Context, paymentGatewayURL string, token string, rideID string, param *paymentGatewayPostPaymentRequest) error {
	b, err := json.Marshal(param)
	if err != nil {
		return err
	}
	deadline := time.Now().Add(8 * time.Second)
	var lastErr error
	for attempt := 0; time.Now().Before(deadline); attempt++ {
		lastErr = func() error {
			req, err := http.NewRequestWithContext(context.WithoutCancel(ctx), http.MethodPost, paymentGatewayURL+"/payments", bytes.NewReader(b))
			if err != nil {
				return err
			}
			req.Header.Set("Content-Type", "application/json")
			req.Header.Set("Authorization", "Bearer "+token)
			req.Header.Set("Idempotency-Key", rideID)
			res, err := paymentClient.Do(req)
			if err != nil {
				return err
			}
			defer res.Body.Close()
			io.Copy(io.Discard, res.Body)
			if res.StatusCode/100 != 2 {
				return fmt.Errorf("payment status %d", res.StatusCode)
			}
			return nil
		}()
		if lastErr == nil {
			return nil
		}
		if attempt >= 3 {
			time.Sleep(5 * time.Millisecond)
		}
	}
	return lastErr
}
