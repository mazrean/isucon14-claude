package main

import (
	"context"
	crand "crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/go-sql-driver/mysql"
	"github.com/jmoiron/sqlx"
	"github.com/oklog/ulid/v2"
)

var (
	db           *sqlx.DB
	shutdownCh   = make(chan struct{})
	errNoFlusher = errors.New("streaming unsupported")
)

type ctxKey int

const (
	ctxUser ctxKey = iota
	ctxOwner
	ctxChair
)

func main() {
	mux := setup()

	srv := &http.Server{Handler: mux, IdleTimeout: 120 * time.Second}
	ln, err := net.Listen("tcp", ":8080")
	if err != nil {
		panic(err)
	}
	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT, syscall.SIGQUIT)
		<-sig
		slog.Info("shutting down")
		close(shutdownCh)
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		srv.Shutdown(ctx)
		flushPersist()
		os.Exit(0)
	}()
	slog.Info("Listening on :8080")
	if err := srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		panic(err)
	}
	select {}
}

func setup() http.Handler {
	host := os.Getenv("ISUCON_DB_HOST")
	if host == "" {
		host = "127.0.0.1"
	}
	port := os.Getenv("ISUCON_DB_PORT")
	if port == "" {
		port = "3306"
	}
	if _, err := strconv.Atoi(port); err != nil {
		panic(fmt.Sprintf("failed to convert DB port number from ISUCON_DB_PORT environment variable into int: %v", err))
	}
	user := os.Getenv("ISUCON_DB_USER")
	if user == "" {
		user = "isucon"
	}
	password := os.Getenv("ISUCON_DB_PASSWORD")
	if password == "" {
		password = "isucon"
	}
	dbname := os.Getenv("ISUCON_DB_NAME")
	if dbname == "" {
		dbname = "isuride"
	}

	dbConfig := mysql.NewConfig()
	dbConfig.User = user
	dbConfig.Passwd = password
	dbConfig.Addr = net.JoinHostPort(host, port)
	dbConfig.Net = "tcp"
	dbConfig.DBName = dbname
	dbConfig.ParseTime = true
	dbConfig.InterpolateParams = true
	dbConfig.Loc = time.UTC

	var err error
	for i := 0; ; i++ {
		db, err = sqlx.Connect("mysql", dbConfig.FormatDSN())
		if err == nil {
			break
		}
		if i > 60 {
			panic(err)
		}
		slog.Error("failed to connect db, retrying", "err", err)
		time.Sleep(time.Second)
	}
	db.SetMaxOpenConns(8)
	db.SetMaxIdleConns(8)

	mu.Lock()
	if err := loadAll(); err != nil {
		panic(err)
	}
	mu.Unlock()

	go persistLoop()
	go matchingLoop()

	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/initialize", postInitialize)

	mux.HandleFunc("POST /api/app/users", appPostUsers)
	mux.Handle("POST /api/app/payment-methods", appAuth(appPostPaymentMethods))
	mux.Handle("GET /api/app/rides", appAuth(appGetRides))
	mux.Handle("POST /api/app/rides", appAuth(appPostRides))
	mux.Handle("POST /api/app/rides/estimated-fare", appAuth(appPostRidesEstimatedFare))
	mux.Handle("POST /api/app/rides/{ride_id}/evaluation", appAuth(appPostRideEvaluatation))
	mux.Handle("GET /api/app/notification", appAuth(appGetNotification))
	mux.Handle("GET /api/app/nearby-chairs", appAuth(appGetNearbyChairs))

	mux.HandleFunc("POST /api/owner/owners", ownerPostOwners)
	mux.Handle("GET /api/owner/sales", ownerAuth(ownerGetSales))
	mux.Handle("GET /api/owner/chairs", ownerAuth(ownerGetChairs))

	mux.HandleFunc("POST /api/chair/chairs", chairPostChairs)
	mux.Handle("POST /api/chair/activity", chairAuth(chairPostActivity))
	mux.Handle("POST /api/chair/coordinate", chairAuth(chairPostCoordinate))
	mux.Handle("GET /api/chair/notification", chairAuth(chairGetNotification))
	mux.Handle("POST /api/chair/rides/{ride_id}/status", chairAuth(chairPostRideStatus))

	mux.HandleFunc("GET /api/internal/matching", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	})

	return mux
}

func appAuth(next http.HandlerFunc) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := r.Cookie("app_session")
		if err != nil || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("app_session cookie is required"))
			return
		}
		mu.Lock()
		user := usersByToken[c.Value]
		mu.Unlock()
		if user == nil {
			writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
			return
		}
		next(w, r.WithContext(context.WithValue(r.Context(), ctxUser, user)))
	})
}

func ownerAuth(next http.HandlerFunc) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := r.Cookie("owner_session")
		if err != nil || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("owner_session cookie is required"))
			return
		}
		mu.Lock()
		owner := ownersByToken[c.Value]
		mu.Unlock()
		if owner == nil {
			writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
			return
		}
		next(w, r.WithContext(context.WithValue(r.Context(), ctxOwner, owner)))
	})
}

func chairAuth(next http.HandlerFunc) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := r.Cookie("chair_session")
		if err != nil || c.Value == "" {
			writeError(w, http.StatusUnauthorized, errors.New("chair_session cookie is required"))
			return
		}
		mu.Lock()
		chair := chairsByToken[c.Value]
		mu.Unlock()
		if chair == nil {
			writeError(w, http.StatusUnauthorized, errors.New("invalid access token"))
			return
		}
		next(w, r.WithContext(context.WithValue(r.Context(), ctxChair, chair)))
	})
}

type postInitializeRequest struct {
	PaymentServer string `json:"payment_server"`
}

type postInitializeResponse struct {
	Language string `json:"language"`
}

func postInitialize(w http.ResponseWriter, r *http.Request) {
	req := &postInitializeRequest{}
	if err := bindJSON(r, req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}

	mu.Lock()
	defer mu.Unlock()
	dropPersistQueue()
	flushMu.Lock()
	out, err := exec.Command("../sql/init.sh").CombinedOutput()
	if err == nil {
		_, err = db.Exec("UPDATE settings SET value = ? WHERE name = 'payment_gateway_url'", req.PaymentServer)
	}
	flushMu.Unlock()
	if err != nil {
		writeError(w, http.StatusInternalServerError, fmt.Errorf("failed to initialize: %s: %w", string(out), err))
		return
	}
	if err := loadAll(); err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	paymentGatewayURL = req.PaymentServer

	writeJSON(w, http.StatusOK, postInitializeResponse{Language: "go"})
}

type Coordinate struct {
	Latitude  int `json:"latitude"`
	Longitude int `json:"longitude"`
}

func bindJSON(r *http.Request, v interface{}) error {
	return json.NewDecoder(r.Body).Decode(v)
}

func writeJSON(w http.ResponseWriter, statusCode int, v interface{}) {
	buf, err := json.Marshal(v)
	if err != nil {
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json;charset=utf-8")
	w.WriteHeader(statusCode)
	w.Write(buf)
}

func writeError(w http.ResponseWriter, statusCode int, err error) {
	w.Header().Set("Content-Type", "application/json;charset=utf-8")
	w.WriteHeader(statusCode)
	buf, _ := json.Marshal(map[string]string{"message": err.Error()})
	w.Write(buf)
}

func secureRandomStr(b int) string {
	k := make([]byte, b)
	if _, err := crand.Read(k); err != nil {
		panic(err)
	}
	return hex.EncodeToString(k)
}

func envDuration(key string, def time.Duration) time.Duration {
	if v := os.Getenv(key); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			return d
		}
	}
	return def
}

func newID() string {
	return ulid.Make().String()
}
