import AppKit

// Entry point. `--smoke` keeps the Phase 1 CLI bridge check available for CI,
// `--selftest` runs the TokenBarCore logic checks; anything else boots the
// menu-bar app (no storyboard, no .app bundle yet).

if CommandLine.arguments.contains("--smoke") {
    exit(Smoke.run())
}
if CommandLine.arguments.contains("--selftest") {
    // Some assertions compare against English UI copy, so on a non-English Mac
    // they would fail for the wrong reason. Say so instead of looking broken.
    if Bundle.main.preferredLocalizations.first != "en" {
        FileHandle.standardError.write(
            Data("warning: re-run with -AppleLanguages \"(en)\" (or `make selftest`) — string assertions expect English\n".utf8))
    }
    SelfTest.run()
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
