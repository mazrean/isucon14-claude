.PHONY: all build deploy bench

BIN ?= webapp/go/isuride
MATCHER ?= 0

all: deploy bench

build:
	cd webapp/go && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -o isuride .

deploy: build
	BIN=$(BIN) MATCHER=$(MATCHER) ./scripts/deploy.sh

bench:
	./scripts/bench.sh
