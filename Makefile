# momentum build helpers.
#
# On macOS the binary must be code-signed with a stable identity for Bluetooth
# (TCC) permission to persist across rebuilds. `make build` signs automatically;
# run `make setup-signing` once first. On Linux/Windows the sign step is a no-op.

CARGO   ?= cargo
PROFILE ?= debug
ARGS    ?=

IDENTITY  ?= momentum-dev
BUNDLE_ID ?= dev.seg6.momentum

ifeq ($(PROFILE),release)
CARGO_FLAGS := --release
else
CARGO_FLAGS :=
endif

BIN := target/$(PROFILE)/momentum
UNAME_S := $(shell uname -s)

.PHONY: build sign run verify-sign setup-signing test clean

build:
	$(CARGO) build $(CARGO_FLAGS)
ifeq ($(UNAME_S),Darwin)
	@$(MAKE) --no-print-directory sign
endif

sign:
ifeq ($(UNAME_S),Darwin)
	codesign --force --sign "$(IDENTITY)" --identifier "$(BUNDLE_ID)" "$(BIN)"
	@echo "Signed $(BIN) as $(IDENTITY) ($(BUNDLE_ID))."
else
	@echo "sign: nothing to do on $(UNAME_S)."
endif

run: build
	"$(BIN)" $(ARGS)

verify-sign:
	codesign -d --verbose=4 "$(BIN)"

setup-signing:
	./scripts/setup-signing.sh

test:
	$(CARGO) test $(CARGO_FLAGS)

clean:
	$(CARGO) clean
