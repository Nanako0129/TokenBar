# Build order matters: the Rust staticlib must exist before swift build links.
# Run everything from the repo root (the -L path in Package.swift is relative).

.PHONY: all rust build run clean check-docs selftest selftest-bundled

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

# The same suite from the configuration that ships: release, inside a .app.
#
# `selftest` above compiles debug and runs the bare executable, so no assertion
# there can observe `Bundle.main.bundleIdentifier` being set — and that is a
# difference a value can be keyed on to be one thing where the suite looks and
# another where it ships. Three source scans were written against that class in
# #146 and all three were escaped, because the gap is not in the source text.
# See the constants in DiscordIPC.swift.
#
# Deliberately NOT the shipping identifier. A bundled run reads the real
# preference domain and this suite does write to `UserDefaults.standard`, so
# the shipping id would put test writes in the installed app's preferences.
# The escape class turns on the identifier being non-nil, not on its value, so
# a throwaway one observes exactly the same thing. The app name is distinct for
# the same reason: this artifact must not be mistaken for — or overwrite — a
# real dist/TokenBar.app.
#
# Not a superset of `selftest`: assertions behind `#if DEBUG` do not exist in
# release, so this run is the smaller one. Both are gates; neither replaces
# the other.
selftest-bundled: rust
	@$(call relink_if_stale,release)
	BUNDLE_ID=com.nyanako.tokenbar.selftest APP_DISPLAY=TokenBarSelfTest scripts/bundle.sh
	dist/TokenBarSelfTest.app/Contents/MacOS/TokenBar --selftest -AppleLanguages "(en)"

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
	cp -R Sources/TokenBar/Resources/Localizations/*.lproj .build/$(1)/
endef
