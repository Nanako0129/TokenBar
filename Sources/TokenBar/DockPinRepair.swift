import AppKit

/// Repoints a Dock tile left behind by the bundle rename.
///
/// The Syrtis rename changes the installed bundle's file name from
/// `TokenBar.app` to its new name (same bundle id, Sparkle normalizes the
/// name), so a tile pinned to `…/TokenBar.app` points at a missing file. On a
/// launch after the rename this rewrites that single tile to the running
/// bundle's URL and restarts the Dock once.
///
/// Source of truth: `com.apple.dock` → `persistent-apps`, read and written
/// through CFPreferences only. The decision is `repair(…)`, a pure function
/// over the tile array; `rewrite(store:…)` is the read/compare/write/read-back
/// sequence over an injected store, so SelfTest can drive both with in-memory
/// values. Only `runIfNeeded()` / `perform()` touch the real Dock domain, the
/// file system (`lstat`, `bookmarkData`) and the Dock process, and SelfTest
/// never calls them (the `--selftest` path in main.swift exits before the app
/// delegate exists).
///
/// This file is skipped whole by the rename script, so the `"TokenBar.app"`
/// literal below survives the rename. Not `#if DEBUG`: `make selftest-bundled`
/// runs the release binary.
enum DockPinRepair {
    /// The pre-rename bundle file name this repair looks for.
    static let oldLeaf = "TokenBar.app"
    /// Recorded before the one Dock restart; a later launch at the same path
    /// never restarts the Dock again.
    static let targetKey = "tokenbar.dockPinRepair.target"

    /// Result of `lstat` on the old path. Only `.missing` (ENOENT) repairs.
    enum Existence { case missing, present, error }

    // The four keys a repair changes; everything else in the tile is kept.
    private static let urlStringKey = "_CFURLString"
    private static let urlStringTypeKey = "_CFURLStringType"
    private static let labelKey = "file-label"
    private static let bookKey = "book"

    // MARK: - Pure decision

    /// Returns the repaired array and whether anything changed. Every string
    /// check (conditions 1–5 and 7) runs before `existence` or `bookmark` is
    /// called, and each is called at most once, for the single candidate.
    static func repair(
        tiles: [Any],
        bundleURL: URL,
        bundleID: String?,
        forced: Bool,
        immutable: Bool,
        existence: (String) -> Existence,
        bookmark: () -> Data?
    ) -> (tiles: [Any], changed: Bool) {
        let unchanged = (tiles: tiles, changed: false)
        // 1. Identity of the running bundle.
        guard let bundleID,
              bundleURL.pathExtension == "app",
              !bundleURL.path.contains("/AppTranslocation/")
        else { return unchanged }
        // 2. The Dock's own locks.
        guard !forced, !immutable else { return unchanged }

        let current = bundleURL.standardizedFileURL
        let currentPath = current.path
        let parentPath = current.deletingLastPathComponent().standardizedFileURL.path

        var candidates: [Int] = []
        for (index, element) in tiles.enumerated() {
            guard let tile = element as? [String: Any],
                  let tileData = tile["tile-data"] as? [String: Any],
                  let url = fileURL(of: tileData)
            else { continue }
            // 7b. Something already points at the running bundle.
            if url.standardizedFileURL.path == currentPath { return unchanged }
            // 3. A file tile for this app, by the running bundle's own id.
            guard tile["tile-type"] as? String == "file-tile",
                  tileData["bundle-identifier"] as? String == bundleID
            else { continue }
            // 4. The old file name.
            guard url.lastPathComponent == oldLeaf else { continue }
            // 5. Same directory as the running bundle.
            guard url.deletingLastPathComponent().standardizedFileURL.path == parentPath
            else { continue }
            candidates.append(index)
        }
        // 7a. Exactly one.
        guard candidates.count == 1,
              let index = candidates.first,
              let tile = tiles[index] as? [String: Any],
              var tileData = tile["tile-data"] as? [String: Any],
              var fileData = tileData["file-data"] as? [String: Any],
              let oldURL = fileURL(of: tileData)
        else { return unchanged }
        // 6. The old path is gone: ENOENT only.
        guard existence(oldURL.standardizedFileURL.path) == .missing else { return unchanged }
        guard let book = bookmark() else { return unchanged }

        fileData[urlStringKey] = URL(fileURLWithPath: currentPath, isDirectory: true).absoluteString
        fileData[urlStringTypeKey] = 15
        tileData["file-data"] = fileData
        tileData[labelKey] = current.deletingPathExtension().lastPathComponent
        tileData[bookKey] = book
        var repaired = tile
        repaired["tile-data"] = tileData
        var out = tiles
        out[index] = repaired
        return (tiles: out, changed: true)
    }

    /// The tile's `file-data._CFURLString` as a file URL, or nil when absent
    /// or not an absolute file URL (a spacer, a malformed tile, a bare path).
    private static func fileURL(of tileData: [String: Any]) -> URL? {
        guard let fileData = tileData["file-data"] as? [String: Any],
              let string = fileData[urlStringKey] as? String,
              let url = URL(string: string), url.isFileURL
        else { return nil }
        return url
    }

    /// True when `candidate` has the same count as `original`, every tile but
    /// at most one is `isEqual`, and that one differs only in the four keys.
    static func differsOnlyInRepairKeys(original: [Any], candidate: [Any]) -> Bool {
        guard original.count == candidate.count else { return false }
        var differing = 0
        for (a, b) in zip(original, candidate) where !(a as AnyObject).isEqual(b) {
            differing += 1
            guard differing == 1,
                  let sa = stripRepairKeys(a), let sb = stripRepairKeys(b),
                  sa.isEqual(sb)
            else { return false }
        }
        return true
    }

    private static func stripRepairKeys(_ element: Any) -> NSDictionary? {
        guard var tile = element as? [String: Any],
              var tileData = tile["tile-data"] as? [String: Any],
              var fileData = tileData["file-data"] as? [String: Any]
        else { return nil }
        fileData[urlStringKey] = nil
        fileData[urlStringTypeKey] = nil
        tileData["file-data"] = fileData
        tileData[labelKey] = nil
        tileData[bookKey] = nil
        tile["tile-data"] = tileData
        return tile as NSDictionary
    }

    /// True only when both reads returned a `mod-count` and they are equal. A
    /// missing value means a concurrent write cannot be ruled out.
    static func modCountUnchanged(_ before: Any?, _ after: Any?) -> Bool {
        guard let before = before as? NSNumber, let after = after as? NSNumber else { return false }
        return before.isEqual(after)
    }

    // MARK: - Read, compare, write, read back

    /// The Dock preference operations `rewrite` needs, injected so SelfTest can
    /// run the sequence against memory.
    struct Store {
        var tiles: () -> [Any]?
        var modCount: () -> Any?
        var synchronize: () -> Bool
        var write: ([Any]) -> Void
    }

    enum Outcome: Equatable {
        case unchanged
        case wrote
        case gaveUp(String)
    }

    static func rewrite(
        store: Store,
        bundleURL: URL,
        bundleID: String?,
        forced: Bool,
        immutable: Bool,
        existence: (String) -> Existence,
        bookmark: () -> Data?
    ) -> Outcome {
        guard let original = store.tiles() else { return .unchanged }
        let before = store.modCount()
        let result = repair(
            tiles: original, bundleURL: bundleURL, bundleID: bundleID,
            forced: forced, immutable: immutable, existence: existence, bookmark: bookmark)
        guard result.changed else { return .unchanged }
        // The Dock may have saved since the read; never write over that.
        guard store.synchronize() else { return .gaveUp("synchronize failed before write") }
        guard modCountUnchanged(before, store.modCount()) else {
            return .gaveUp("Dock preferences changed while repairing")
        }
        store.write(result.tiles)
        guard store.synchronize() else { return .gaveUp("synchronize failed after write") }
        guard let readBack = store.tiles(),
              (readBack as NSArray).isEqual(result.tiles as NSArray),
              differsOnlyInRepairKeys(original: original, candidate: readBack)
        else { return .gaveUp("read-back does not match the written array") }
        return .wrote
    }

    // MARK: - I/O (never reached from SelfTest)

    private static var dockDomain: CFString { "com.apple.dock" as CFString }
    private static var appsKey: CFString { "persistent-apps" as CFString }
    private static var modCountKey: CFString { "mod-count" as CFString }

    /// Called once from `applicationDidFinishLaunching`; does its work on a
    /// background queue and never affects the app on failure.
    static func runIfNeeded() {
        let bundleURL = Bundle.main.bundleURL
        let bundleID = Bundle.main.bundleIdentifier
        DispatchQueue.global(qos: .utility).async {
            perform(bundleURL: bundleURL, bundleID: bundleID)
        }
    }

    private static func perform(bundleURL: URL, bundleID: String?) {
        let store = Store(
            tiles: { CFPreferencesCopyAppValue(appsKey, dockDomain) as? [Any] },
            modCount: { CFPreferencesCopyAppValue(modCountKey, dockDomain) },
            synchronize: { CFPreferencesAppSynchronize(dockDomain) },
            write: { CFPreferencesSetAppValue(appsKey, $0 as CFArray, dockDomain) })
        let outcome = rewrite(
            store: store, bundleURL: bundleURL, bundleID: bundleID,
            forced: CFPreferencesAppValueIsForced(appsKey, dockDomain),
            immutable: CFPreferencesGetAppBooleanValue("contents-immutable" as CFString, dockDomain, nil)
                || CFPreferencesGetAppBooleanValue("static-only" as CFString, dockDomain, nil),
            existence: { path in
                var info = stat()
                if lstat(path, &info) == 0 { return .present }
                return errno == ENOENT ? .missing : .error
            },
            bookmark: {
                do { return try bundleURL.bookmarkData() } catch {
                    NSLog("TokenBar: Dock pin repair gave up: bookmark failed: \(error)")
                    return nil
                }
            })
        switch outcome {
        case .unchanged:
            return
        case .gaveUp(let reason):
            NSLog("TokenBar: Dock pin repair gave up: \(reason)")
            return
        case .wrote:
            break
        }

        let path = bundleURL.standardizedFileURL.path
        let defaults = UserDefaults.standard
        if defaults.string(forKey: targetKey) == path {
            NSLog("TokenBar: Dock pin repaired again for \(path); not restarting the Dock a second time")
            return
        }
        defaults.set(path, forKey: targetKey)
        guard let pid = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.dock")
            .first?.processIdentifier, pid > 0
        else {
            NSLog("TokenBar: Dock pin repair gave up: Dock process not found")
            return
        }
        if kill(pid, SIGTERM) != 0 {
            NSLog("TokenBar: Dock pin repair gave up: could not restart the Dock (errno \(errno))")
        }
    }

    // MARK: - SelfTest fixtures (in-memory only)

    static func selfTest(_ expect: (Bool, String) -> Void) {
        let id = "com.nyanako.tokenbar"
        let running = URL(fileURLWithPath: "/Applications/Syrtis.app", isDirectory: true)
        let book = Data([0x62, 0x6F, 0x6F, 0x6B])

        func tile(_ url: Any, id bundleID: String = id, type: String = "file-tile") -> [String: Any] {
            [
                "GUID": 12345,
                "tile-type": type,
                "tile-data": [
                    "bundle-identifier": bundleID,
                    "file-label": "TokenBar",
                    "file-type": 41,
                    "book": Data([0x01]),
                    "file-data": ["_CFURLString": url, "_CFURLStringType": 15] as [String: Any],
                ] as [String: Any],
            ]
        }
        let old = tile("file:///Applications/TokenBar.app/")
        let other = tile("file:///System/Applications/Mail.app/", id: "com.apple.mail")
        let spacer: [String: Any] = ["tile-type": "spacer-tile", "tile-data": [String: Any]()]

        func run(
            _ tiles: [Any], url: URL = running, bundleID: String? = id,
            forced: Bool = false, immutable: Bool = false, exists: Existence = .missing
        ) -> (tiles: [Any], changed: Bool) {
            repair(tiles: tiles, bundleURL: url, bundleID: bundleID, forced: forced,
                   immutable: immutable, existence: { _ in exists }, bookmark: { book })
        }
        func leftAlone(_ label: String, _ result: (tiles: [Any], changed: Bool), _ input: [Any]) {
            expect(!result.changed && (result.tiles as NSArray).isEqual(input as NSArray),
                "DOCK-PIN leaves the Dock alone: \(label)")
        }

        // Control: the matching fixture repairs, so every "left alone" case
        // below is refused by its one guard and not by a broken fixture.
        let input: [Any] = [other, old, spacer]
        let fixed = run(input)
        expect(fixed.changed, "DOCK-PIN repairs the one tile pinned to the missing TokenBar.app")
        let fixedData = (fixed.tiles[1] as? [String: Any])?["tile-data"] as? [String: Any]
        let fixedFile = fixedData?["file-data"] as? [String: Any]
        expect(fixedFile?["_CFURLString"] as? String == "file:///Applications/Syrtis.app/"
                && fixedFile?["_CFURLStringType"] as? Int == 15
                && fixedData?["file-label"] as? String == "Syrtis"
                && fixedData?["book"] as? Data == book,
            "DOCK-PIN the repaired tile carries the running bundle's URL, type 15, label and bookmark")
        expect(fixed.tiles.count == input.count
                && (fixed.tiles[0] as AnyObject).isEqual(other)
                && (fixed.tiles[2] as AnyObject).isEqual(spacer),
            "DOCK-PIN output keeps the count and every other tile isEqual")
        expect(differsOnlyInRepairKeys(original: input, candidate: fixed.tiles),
            "DOCK-PIN the repaired tile differs from the original only in the four keys")
        var tampered = fixed.tiles
        var tamperedTile = tampered[1] as! [String: Any]
        tamperedTile["GUID"] = 1
        tampered[1] = tamperedTile
        expect(!differsOnlyInRepairKeys(original: input, candidate: tampered)
                && !differsOnlyInRepairKeys(original: input, candidate: Array(fixed.tiles.dropLast())),
            "DOCK-PIN the shape check rejects a fifth changed key and a changed count (control)")
        expect((try? PropertyListSerialization.data(
                    fromPropertyList: fixed.tiles, format: .binary, options: 0)) != nil,
            "DOCK-PIN the repaired array serializes as a binary property list")

        leftAlone("the old path is present", run(input, exists: .present), input)
        leftAlone("lstat fails with something other than ENOENT", run(input, exists: .error), input)
        let foreign: [Any] = [tile("file:///Applications/TokenBar.app/", id: "com.example.other")]
        leftAlone("the tile's bundle id is not the running bundle's", run(foreign), foreign)
        leftAlone("the tile is not a file-tile", run([tile("file:///Applications/TokenBar.app/", type: "directory-tile")]),
                  [tile("file:///Applications/TokenBar.app/", type: "directory-tile")])
        leftAlone("no tile matches", run([other, spacer]), [other, spacer])
        leftAlone("two tiles match", run([old, other, old]), [old, other, old])
        let both: [Any] = [old, tile("file:///Applications/Syrtis.app/")]
        leftAlone("a tile already points at the running bundle", run(both), both)
        let renamed: [Any] = [tile("file:///Applications/Other.app/")]
        leftAlone("the old leaf is not TokenBar.app", run(renamed), renamed)
        let elsewhere: [Any] = [tile("file:///Users/someone/Applications/TokenBar.app/")]
        leftAlone("the old tile is in a different directory", run(elsewhere), elsewhere)
        leftAlone("the running bundle has no bundle id", run(input, bundleID: nil), input)
        let bare: [Any] = [tile("file:///opt/build/TokenBar.app/")]
        leftAlone("the running bundle is not an .app",
                  run(bare, url: URL(fileURLWithPath: "/opt/build/Syrtis", isDirectory: true)), bare)
        let translocatedDir = "/private/var/folders/ab/AppTranslocation/XYZ/d"
        let translocated: [Any] = [tile("file://\(translocatedDir)/TokenBar.app/")]
        leftAlone("the running bundle is translocated",
                  run(translocated, url: URL(fileURLWithPath: "\(translocatedDir)/Syrtis.app", isDirectory: true)),
                  translocated)
        leftAlone("persistent-apps is forced", run(input, forced: true), input)
        leftAlone("the Dock is contents-immutable or static-only", run(input, immutable: true), input)

        // Spacers and malformed tiles are skipped, never trapped on, and do not
        // stop the one real match from being repaired.
        let malformed: [Any] = [
            "not a tile", 42, spacer, ["tile-type": "file-tile"] as [String: Any],
            ["tile-type": "file-tile", "tile-data": ["x", "y"]] as [String: Any],
            ["tile-type": "file-tile", "tile-data": ["file-data": ["_CFURLString": 7]]] as [String: Any],
            ["tile-type": "file-tile",
             "tile-data": ["file-data": ["_CFURLString": "/Applications/TokenBar.app"]]] as [String: Any],
            tile(9),
        ]
        leftAlone("only spacers and malformed tiles", run(malformed), malformed)
        let mixed = malformed + [old]
        let mixedFixed = run(mixed)
        expect(mixedFixed.changed && differsOnlyInRepairKeys(original: mixed, candidate: mixedFixed.tiles),
            "DOCK-PIN spacers and malformed tiles do not stop the one real match")

        // Order: no file-system call for a tile the string checks refuse.
        var probed = 0
        _ = repair(tiles: [other, spacer], bundleURL: running, bundleID: id, forced: false,
                   immutable: false, existence: { _ in probed += 1; return .missing },
                   bookmark: { probed += 1; return book })
        expect(probed == 0, "DOCK-PIN no lstat or bookmark before the string checks pass")

        // The write sequence against an in-memory store.
        final class Memory {
            var tiles: [Any]
            var modCount = 7
            var bumpOnSync = false
            var writes = 0
            init(_ tiles: [Any]) { self.tiles = tiles }
        }
        func sequence(_ memory: Memory) -> Outcome {
            let store = Store(
                tiles: { memory.tiles },
                modCount: { memory.modCount },
                synchronize: {
                    if memory.bumpOnSync { memory.modCount += 1; memory.bumpOnSync = false }
                    return true
                },
                write: { memory.tiles = $0; memory.writes += 1 })
            return rewrite(store: store, bundleURL: running, bundleID: id, forced: false,
                           immutable: false, existence: { _ in .missing }, bookmark: { book })
        }
        let clean = Memory(input)
        expect(sequence(clean) == .wrote && clean.writes == 1
                && (clean.tiles as NSArray).isEqual(fixed.tiles as NSArray),
            "DOCK-PIN the write sequence writes the repaired array once and reads it back")
        let raced = Memory(input)
        raced.bumpOnSync = true
        expect(sequence(raced) == .gaveUp("Dock preferences changed while repairing") && raced.writes == 0
                && (raced.tiles as NSArray).isEqual(input as NSArray),
            "DOCK-PIN a mod-count change between read and write abandons without writing")
        expect(!modCountUnchanged(nil, nil) && !modCountUnchanged(3, nil) && modCountUnchanged(3, 3),
            "DOCK-PIN a missing mod-count counts as changed")
    }
}
