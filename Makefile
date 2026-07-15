# ChainView toolchain entry points.
#
# `make pre-push` is the canonical local gate (TESTING.md §10): it runs
# fix + fmt + lint-fix + test + doc + check-spanish. The four non-negotiable
# commands
# (`lint`, `fmt-check`, `test`, `build-release`) re-run individually in CI.

CARGO := cargo

.PHONY: all fix fmt fmt-check lint lint-fix test build build-release doc check-spanish pre-push

all: build

fix:
	$(CARGO) fix --allow-dirty --allow-staged --all-targets --all-features

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

lint:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

lint-fix:
	$(CARGO) clippy --fix --allow-dirty --allow-staged --all-targets --all-features -- -D warnings

test:
	$(CARGO) test --all-features

build:
	$(CARGO) build

build-release:
	$(CARGO) build --release

doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps --all-features

check-spanish:
	@if grep -rnE --include='*.rs' '[áéíóúÁÉÍÓÚñÑ¿¡]' src tests benches 2>/dev/null; then \
		echo 'error: Spanish text found in code or comments'; exit 1; \
	else \
		echo 'check-spanish: OK'; \
	fi

pre-push: fix fmt lint-fix test doc check-spanish
