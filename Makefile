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
# Not a superset of `selftest`: assertions behind `#if DEBUG` do not exist in
# release, so this run is the smaller one. Both are gates; neither replaces
# the other.
#
# The identity below is the one knob, and it trades two hazards against each
# other. A bundled run resolves `UserDefaults.standard` to whatever domain the
# identifier names, and this suite does write there — `PopoverChrome.heightKey`
# and the dashboard year key are staged and restored, because the production
# types read `.standard` directly. Under the shipping identifier those writes
# land in the installed app's own preferences.
#
# So the default is a throwaway, and that is the WEAKER gate: it catches a value
# keyed on the identifier being nil, but not one keyed on the production string
# itself, which would take the safe branch here and the other branch only once
# installed. CI closes that by passing the empty override, which is what the
# push-to-main gate actually runs — see .github/workflows/ci.yml. An ephemeral
# runner has no installation to pollute; a developer's Mac does.
#
# Empty means "whatever scripts/bundle.sh defaults to", which IS the shipping
# identifier — deliberately not spelled out again here, so a future rename has
# one place to change and cannot silently weaken this gate to a string that no
# longer matches.
#
# The app NAME is not a second knob. It was, briefly, and it reintroduced the
# same escape one attribute over: `CFBundleName == "TokenBar"`, or a check on
# the bundle URL ending in `TokenBar.app`, would have taken the safe branch in a
# gate whose bundle was called something else. So this assembles a real
# `TokenBar.app` and keeps it out of the way by directory instead — OUT_DIR, not
# APP_DISPLAY — which also still leaves a hand-built `dist/TokenBar.app` alone.
#
# What remains different from a released bundle, stated rather than discovered
# one review round at a time: the install path (`dist/selftest/` and not
# `/Applications/`), the version and build number (bundle.sh's defaults, since
# release.yml passes the real ones), and the signature (ad-hoc, not Developer
# ID). A value keyed on any of those is outside what this gate can observe, and
# no arrangement of a locally assembled bundle closes that — only installing a
# notarized build would. The three that ARE observable here are the identifier,
# the name, and the release configuration itself.
SELFTEST_BUNDLE_ID ?= com.nyanako.tokenbar.selftest
selftest-bundled: rust
	@$(call relink_if_stale,release)
	BUNDLE_ID=$(SELFTEST_BUNDLE_ID) OUT_DIR=dist/selftest scripts/bundle.sh
	dist/selftest/TokenBar.app/Contents/MacOS/TokenBar --selftest -AppleLanguages "(en)"

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
