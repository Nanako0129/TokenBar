# Build order matters: the Rust staticlib must exist before swift build links.
# Run everything from the repo root (the -L path in Package.swift is relative).

.PHONY: all rust build run clean check-docs

all: build

check-docs:
	python3 scripts/check_knowledge.py

rust:
	cargo build --release

build: rust
	@$(call relink_if_stale,debug)
	swift build

run: rust
	@$(call relink_if_stale,debug)
	swift run TokenBar

clean:
	cargo clean
	swift package clean

bundle: rust
	@$(call relink_if_stale,release)
	swift build -c release
	scripts/bundle.sh

# SwiftPM does not track the Rust staticlib as a dependency: with no Swift
# source changes it reuses the cached executable and silently ships stale
# Rust code. Drop the executable whenever the staticlib is newer.
define relink_if_stale
	if [ target/release/libtb_core_ffi.a -nt .build/$(1)/TokenBar ]; then \
		rm -f .build/$(1)/TokenBar; \
	fi
endef
