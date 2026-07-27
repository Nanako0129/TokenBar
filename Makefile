# Build order matters: the Rust staticlib must exist before swift build links.
# Run everything from the repo root (the -L path in Package.swift is relative).

.PHONY: all rust build run clean check-docs selftest

all: build

check-docs:
	python3 scripts/check_knowledge.py

rust:
	cargo build --release

build: rust
	@$(call relink_if_stale,debug)
	swift build
	@$(call sync_localizations,debug)

# Depends on `build`, not `rust`: the localization sync copies into
# .build/debug, which only SwiftPM creates. Running it before a build fails on a
# fresh or freshly cleaned checkout.
run: build
	swift run TokenBar

# Several assertions compare against English UI copy, so the language is
# pinned here rather than inherited from the developer's Mac.
selftest: build
	swift run TokenBar --selftest -AppleLanguages "(en)"

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

# Translations are read from Bundle.main, which for a bare `swift run` is the
# directory holding the executable — not the SwiftPM resource bundle. Copying
# them next to the binary is what makes a dev run show the same strings the
# .app does. `scripts/bundle.sh` installs the same .lproj into the real app.
define sync_localizations
	cp -R Resources/Localizations/*.lproj .build/$(1)/
endef
