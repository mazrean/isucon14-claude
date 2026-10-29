package main

import (
	"net/http"
	_ "net/http/pprof"
	"os"
)

func init() {
	if os.Getenv("PPROF") != "" {
		go http.ListenAndServe("127.0.0.1:6060", nil)
	}
}
