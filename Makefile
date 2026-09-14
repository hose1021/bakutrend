# Commands for building, checking and shipping bakutrend.
#
# Override the toolchain when cargo is not on your PATH:
#   make CARGO=/Users/you/.cargo/bin/cargo test
# Serve somewhere else:
#   make serve BIND=0.0.0.0:9000

CARGO  ?= cargo
PREFIX ?= $(HOME)/.local
BIND   ?= 127.0.0.1:8080
SITE   ?= site

.DEFAULT_GOAL := help
.PHONY: help build release run serve poll site test fmt fmt-check clippy lint ci install clean

help:
	@echo "build      compile a debug binary"
	@echo "release    compile the optimized binary"
	@echo "run        start the TUI"
	@echo "serve      start the web UI at http://$(BIND) (override: BIND=host:port)"
	@echo "poll       poller loop, no UI"
	@echo "site       poll once, then write a static snapshot into site/ (override: SITE=dir)"
	@echo "test       run every test"
	@echo "fmt        format the source"
	@echo "fmt-check  fail when the source is not formatted"
	@echo "clippy     lint; warnings are errors"
	@echo "lint       fmt-check and clippy"
	@echo "ci         the gates a commit must pass: lint and test"
	@echo "install    copy the release binary to $(PREFIX)/bin"
	@echo "clean      remove the build directory"

build:
	$(CARGO) build

release:
	$(CARGO) build --release

run:
	$(CARGO) run

serve:
	$(CARGO) run -- --bind $(BIND)

poll:
	$(CARGO) run -- --poll-only

site:
	$(CARGO) run --release -- --export $(SITE)

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

lint: fmt-check clippy

ci: lint test

install: release
	install -d $(PREFIX)/bin
	install -m 0755 target/release/bakutrend $(PREFIX)/bin/bakutrend

clean:
	$(CARGO) clean
