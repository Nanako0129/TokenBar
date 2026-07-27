// swift-tools-version: 6.0
import PackageDescription

// The Rust staticlib must be built first: `cargo build --release` (or `make`).
// `swift build` must run from the repo root so the relative -L path resolves.
let package = Package(
    name: "TokenBar",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "crosscheck-harness", targets: ["CrossCheckHarness"]),
    ],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.6.0"),
    ],
    targets: [
        .target(name: "CTB", path: "Sources/CTB"),
        .target(
            name: "TokenBarCore",
            dependencies: ["CTB"],
            path: "Sources/TokenBarCore"
        ),
        .executableTarget(
            name: "TokenBar",
            dependencies: [
                "TokenBarCore",
                .product(name: "Sparkle", package: "Sparkle"),
            ],
            path: "Sources/TokenBar",
            resources: [
                .copy("Resources/agent-icons"),
                .copy("Resources/anim-cat2"),
                .copy("Resources/anim-cat2-light"),
                .copy("Resources/anim-parrot"),
                .copy("Resources/anim-parrot-light"),
                .copy("Resources/third-party"),
                // Keep the .lproj directories at the package resource-bundle
                // root so direct `swift run` can stage them beside the
                // executable for Bundle.main/SwiftUI lookup.
                .copy("Resources/Localizations/en.lproj"),
                .copy("Resources/Localizations/zh-Hant.lproj"),
            ],
            linkerSettings: rustLinkerSettings
        ),
        // Swift↔C# fixture cross-check harness. Depends on TokenBarCore (which
        // links the Rust staticlib), so it needs the same rustLinkerSettings.
        // Format.swift is symlinked in from Sources/TokenBar so the harness
        // compiles the exact shipping formatter source, not a reimplementation.
        .executableTarget(
            name: "CrossCheckHarness",
            dependencies: ["TokenBarCore"],
            path: "Sources/CrossCheckHarness",
            linkerSettings: rustLinkerSettings
        ),
    ]
)

// The Rust staticlib must already exist (cargo build --release) and the link
// must run from the repo root for the relative -L path to resolve.
var rustLinkerSettings: [LinkerSetting] {
    [
        // The archive is named as an explicit linker input, NOT -l: since the
        // crate also builds a cdylib for the Windows port's P/Invoke, both
        // libtb_core_ffi.a and .dylib sit in target/release, and Darwin's
        // -l lookup prefers the dylib — which would silently turn the app's
        // Rust linkage dynamic with a build-tree absolute path (Codex review
        // finding, verified with otool -L).
        .unsafeFlags(["-Xlinker", "target/release/libtb_core_ffi.a"]),
        // Sparkle.framework rides in Contents/Frameworks inside the .app.
        .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]),
        .linkedFramework("Security"),
        .linkedFramework("SystemConfiguration"),
        .linkedFramework("CoreFoundation"),
        .linkedLibrary("c++"),
        .linkedLibrary("resolv"),
    ]
}
