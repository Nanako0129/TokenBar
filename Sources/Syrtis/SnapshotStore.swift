import Darwin
import Foundation
import TokenBarCore

/// Disk persistence for the reopen dashboard, so a relaunch renders the previous
/// graph immediately instead of the loading state.
///
/// Deliberately separate from `DashboardSnapshot`, which carries `agentUsage`
/// (account identity, quota, provider errors) and `trace`. That struct must
/// never become `Codable`; this envelope structurally cannot hold those fields,
/// which is a stronger guarantee than remembering not to encode them.
///
/// The model report is not persisted. Keying it on the graph generation would
/// freeze prices when logs are idle (issue #191); re-requesting it every launch
/// costs one deferred scan the model lenses already pay for on a cold open.
struct SnapshotEnvelope: Codable, Sendable {
    /// Bump on any shape change. A mismatch is a cold fallback, never a migration.
    static let schemaVersion = 2

    let snapshotSchemaVersion: Int
    let bundleIdentifier: String
    let shortVersion: String
    let buildNumber: String
    let savedAt: Date
    /// nil means the all-time slice, matching `DashboardModel.year`.
    let selectedYear: String?
    let payload: UsagePayload
    let knownYears: [String]
    /// The scan roots this payload was produced under — the exact JSON handed
    /// to the engine, so the recorded set and the scanned set cannot differ in
    /// expansion, ordering, or deduplication.
    ///
    /// Everything else in this envelope identifies the BUILD. Nothing
    /// identified the INPUTS, so editing the scan roots invalidated the
    /// in-memory caches and left this file passing every check: the next open
    /// restored totals from a removed root, or without an added one, and
    /// published them as `.ready` until a fresh scan finished.
    ///
    /// A recorded fingerprint rather than deleting the file on the edit. The
    /// edit is not the only way the roots can differ from the ones a snapshot
    /// was built under — the app can be killed between the two, and the
    /// registry is UserDefaults, which can move while the app is not running.
    /// A check at restore covers every route; a delete covers one.
    let scanRoots: String

    // Explicit for the same reason as the graph types: a field added later must
    // fail the exact-key test rather than being written to disk silently.
    enum CodingKeys: String, CodingKey {
        case snapshotSchemaVersion
        case bundleIdentifier
        case shortVersion
        case buildNumber
        case savedAt
        case selectedYear
        case payload
        case knownYears
        case scanRoots
    }
}

/// Reads and writes the envelope under a verified, owner-only directory.
///
/// Threat model, unchanged from the rest of this app: other local accounts,
/// accidental symlink or path substitution, and malformed local files. A
/// same-UID attacker is explicitly out of scope.
///
/// Durability is explicitly NOT claimed. This is a disposable cache, so no
/// `fsync` is issued and any post-crash corruption is a cold fallback. The
/// credential-store durability machinery is deliberately not inherited.
enum SnapshotStore {
    /// The measured envelope on a real corpus is ~117 KB for 108 active days,
    /// roughly 1.1 KB per day with data. This cap exists to bound a hostile or
    /// corrupt read, not to reject legitimate growth: 16 MB is about forty years
    /// of dense daily usage, so it can never fire on real data while still
    /// keeping a malformed file from being read into memory unbounded.
    static let byteCap = 16 * 1024 * 1024
    /// Same rationale: bounds a malformed list without rejecting real data.
    static let maxKnownYears = 200
    /// A snapshot claiming to be from the future is a clock or tamper artifact.
    static let futureSkewAllowance: TimeInterval = 60 * 60
    /// Older than this is not worth rendering even if everything else matches.
    static let maxAge: TimeInterval = 60 * 60 * 24 * 90

    private static let fileName = "dashboard-snapshot.json"
    private static let directoryName = "SyrtisDashboardSnapshot"

    // MARK: - Location

    /// Injectable so tests never touch the production location. Production
    /// resolves the caches directory; tests pass their own temporary URL.
    static func defaultDirectory() -> URL? {
        try? FileManager.default.url(
            for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: false
        ).appendingPathComponent(directoryName, isDirectory: true)
    }

    // MARK: - Directory handle

    /// Opens the app subdirectory and returns a descriptor every later operation
    /// is performed against, so a directory swapped after verification cannot
    /// redirect the read or the write.
    ///
    /// `O_NOFOLLOW` constrains only the FINAL path component. Intermediate
    /// components of the caches path are still followed; they are trusted
    /// because they lie inside the user's own home directory, which matches the
    /// threat model above.
    private static func openDirectory(_ url: URL, createIfMissing: Bool) -> Int32? {
        let path = url.path
        if createIfMissing {
            // 0700 at creation; an existing directory keeps whatever it has and
            // is rejected below if that is not owner-only.
            _ = mkdir(path, S_IRWXU)
        }
        let fd = open(path, O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { return nil }
        var info = stat()
        guard fstat(fd, &info) == 0,
              (info.st_mode & S_IFMT) == S_IFDIR,
              info.st_uid == getuid(),
              (info.st_mode & 0o077) == 0
        else {
            close(fd)
            return nil
        }
        return fd
    }

    // MARK: - Read

    /// Returns the raw bytes, or nil for every rejection: missing, symlinked,
    /// non-regular, wrong owner, or larger than the cap. Never throws to the
    /// caller — a snapshot that cannot be trusted is simply absent.
    static func readBytes(in directory: URL) -> Data? {
        guard let dirfd = openDirectory(directory, createIfMissing: false) else { return nil }
        defer { close(dirfd) }
        let fd = openat(dirfd, fileName, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard fd >= 0 else { return nil }
        defer { close(fd) }
        // fstat the same descriptor the bytes come from, so nothing can be
        // swapped between the check and the read.
        var info = stat()
        guard fstat(fd, &info) == 0,
              (info.st_mode & S_IFMT) == S_IFREG,
              info.st_uid == getuid(),
              info.st_size <= byteCap
        else { return nil }
        // Size from the same fstat, plus one byte: a file that grew between the
        // check and the read overflows that slot and is rejected rather than
        // silently truncated. `remaining` is hoisted because reading
        // `buffer.count` inside the closure would overlap the exclusive borrow.
        let capacity = min(Int(info.st_size), byteCap) + 1
        var buffer = [UInt8](repeating: 0, count: capacity)
        var filled = 0
        while filled < capacity {
            let remaining = capacity - filled
            let offset = filled
            let n: Int = buffer.withUnsafeMutableBytes { raw in
                read(fd, raw.baseAddress!.advanced(by: offset), remaining)
            }
            if n == 0 { break }
            if n < 0 {
                if errno == EINTR { continue }
                return nil
            }
            filled += n
        }
        guard filled <= byteCap else { return nil }
        return Data(buffer[0..<filled])
    }

    // MARK: - Write

    /// Encodes, then replaces atomically within the verified directory. Any
    /// failure before the rename leaves the previous snapshot untouched.
    @discardableResult
    static func write(_ bytes: Data, in directory: URL) -> Bool {
        guard bytes.count <= byteCap else { return false }
        guard let dirfd = openDirectory(directory, createIfMissing: true) else { return false }
        defer { close(dirfd) }

        let temp = ".\(fileName).\(getpid()).\(UInt32.random(in: 0..<UInt32.max)).tmp"
        // O_WRONLY because this descriptor is written; mode 0600 at creation
        // rather than write-then-chmod, which would leave a readable window.
        let fd = openat(dirfd, temp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard fd >= 0 else { return false }

        var wrote = 0
        var ok = true
        bytes.withUnsafeBytes { raw in
            while wrote < raw.count {
                let n = Darwin.write(
                    fd, raw.baseAddress!.advanced(by: wrote), raw.count - wrote)
                if n < 0 {
                    if errno == EINTR { continue }
                    ok = false
                    return
                }
                if n == 0 { ok = false; return }
                wrote += n
            }
        }
        close(fd)
        guard ok, wrote == bytes.count else {
            _ = unlinkat(dirfd, temp, 0)
            return false
        }
        guard renameat(dirfd, temp, dirfd, fileName) == 0 else {
            _ = unlinkat(dirfd, temp, 0)
            return false
        }
        return true
    }

    // MARK: - Validation

    /// Every predicate that must hold before a decoded envelope may be shown.
    /// Rejects rather than partially restoring: a snapshot is either coherent or
    /// absent.
    /// `scanRoots` is the root payload to check against, and it must match
    /// exactly: a snapshot built under a different input set is not this
    /// process's answer, whatever else about it still lines up.
    ///
    /// Defaulted to the LAST APPLIED registry rather than to the empty set, so
    /// a call site that forgets the argument is conservative instead of wrong —
    /// the empty default would have quietly accepted every snapshot on a
    /// machine that has extra roots configured, which is the exact case the
    /// field exists for.
    ///
    /// Last applied rather than currently applied: a restore runs before the
    /// launch-time `ClaudeExtraRoots.apply` lands, so the in-memory value is
    /// still empty and comparing against it would reject every snapshot on
    /// every launch. Last applied rather than configured, because a path the
    /// setter REFUSED is in the configured list and was never in the scan.
    static func validate(
        _ envelope: SnapshotEnvelope,
        expectedYear: String?,
        identity: BuildIdentity,
        scanRoots: String = ClaudeExtraRoots.appliedPayloadJSON,
        now: Date = Date()
    ) -> Bool {
        guard envelope.snapshotSchemaVersion == SnapshotEnvelope.schemaVersion,
              envelope.bundleIdentifier == identity.bundleIdentifier,
              envelope.shortVersion == identity.shortVersion,
              envelope.buildNumber == identity.buildNumber,
              envelope.scanRoots == scanRoots
        else { return false }

        // The year this process will actually request, not the one that was
        // selected when the snapshot was written.
        guard envelope.selectedYear == expectedYear else { return false }
        if let year = envelope.selectedYear {
            guard isFourDigitYear(year),
                  envelope.payload.years.contains(where: { $0.year == year }),
                  envelope.payload.contributions.allSatisfy({ $0.date.hasPrefix(year) })
            else { return false }
        }

        guard envelope.knownYears.count <= maxKnownYears,
              envelope.knownYears.allSatisfy(isFourDigitYear),
              Set(envelope.knownYears).count == envelope.knownYears.count,
              envelope.knownYears == envelope.knownYears.sorted(by: >)
        else { return false }
        // knownYears is a union that never drops a year, so it must cover the
        // payload rather than equal it.
        guard Set(envelope.payload.years.map(\.year)).isSubset(of: Set(envelope.knownYears))
        else { return false }

        // Every persisted civil date, not only the ones a year filter covers.
        // An all-time snapshot skipped this entirely before, which is the shape
        // a malformed file would take.
        let range = envelope.payload.meta.dateRange
        guard isBoundedCivilDate(range.start), isBoundedCivilDate(range.end),
              range.start <= range.end
        else { return false }
        for year in envelope.payload.years {
            guard isFourDigitYear(year.year),
                  isBoundedCivilDate(year.range.start), isBoundedCivilDate(year.range.end),
                  year.range.start <= year.range.end
            else { return false }
        }
        for contribution in envelope.payload.contributions {
            guard isBoundedCivilDate(contribution.date) else { return false }
        }

        let age = now.timeIntervalSince(envelope.savedAt)
        guard age > -futureSkewAllowance, age < maxAge else { return false }
        return true
    }

    /// A `YYYY-MM-DD` date inside an era the rest of the app can compute on.
    ///
    /// `ISODay.init?` parses with `Int` and then multiplies — `era * 146097` on
    /// a year like `9223372036854775807` overflows and TRAPS, so a malformed
    /// file on disk could crash the dashboard at launch. Less extreme but still
    /// huge spans make the streak computation walk an impractical number of
    /// days. Bounding the era closes both, and it is the payload's own dates
    /// that need it, not just the ones the year filter happens to cover.
    static let earliestYear = 1970
    static let latestYear = 2200

    static func isBoundedCivilDate(_ value: String) -> Bool {
        let parts = value.split(separator: "-", omittingEmptySubsequences: false)
        guard parts.count == 3,
              parts[0].count == 4, parts[1].count == 2, parts[2].count == 2,
              value.allSatisfy({ $0 == "-" || ($0.isASCII && $0.isNumber) }),
              let year = Int(parts[0]), let month = Int(parts[1]), let day = Int(parts[2])
        else { return false }
        return (earliestYear...latestYear).contains(year)
            && (1...12).contains(month) && (1...31).contains(day)
    }

    private static func isFourDigitYear(_ value: String) -> Bool {
        value.count == 4 && value.allSatisfy { $0.isASCII && $0.isNumber }
    }
}

/// The exact build a snapshot must have been written by. Any difference is a
/// cold fallback: the first launch after an update deliberately reloads rather
/// than rendering a payload an older shape produced.
struct BuildIdentity: Equatable, Sendable {
    let bundleIdentifier: String
    let shortVersion: String
    let buildNumber: String

    /// The shipping identity. Returns nil for anything that is not the official
    /// bundle — including `swift run`, which has no bundle identifier — so a
    /// development or test binary can never read or write the production file.
    /// Runtime modes that enter the app but are not a user session.
    ///
    /// The identifier alone is not enough. `scripts/bundle.sh` stamps the
    /// production identifier by default, and running `--selftest` or `--smoke`
    /// on a release bundle is an ordinary way to verify one — at which point
    /// every fixture-backed `DashboardModel(cachesSnapshot: true)` in the suite
    /// would resolve the real snapshot directory and could overwrite a user's
    /// dashboard with fabricated usage. `SELFTEST_BUNDLE_ID` defaults to a
    /// distinct identifier, but it is a `?=` override, so it cannot be the only
    /// thing standing between fixtures and the production file.
    ///
    /// This is the same set the isolation spy asserts against; before, the test
    /// enumerated these modes while the production guard did not know them.
    static let nonUserRuntimeFlags = ["--selftest", "--smoke", "--demo", "--icon-gallery"]

    /// Whether these launch arguments describe something other than a user
    /// session. Split out so it is directly assertable — the identifier half
    /// cannot be exercised under `swift run`, which has no bundle at all.
    static func isNonUserRuntime(_ arguments: [String]) -> Bool {
        arguments.contains { nonUserRuntimeFlags.contains($0) }
    }

    /// Every input is injectable, and deliberately so. Under `swift run` there
    /// is no bundle at all, so a test that only overrides `arguments` proves
    /// nothing: the result is nil for the bundle reason whatever the flags say.
    /// Removing the runtime check below survived exactly that mutation once.
    static func shipping(
        arguments: [String] = CommandLine.arguments,
        bundleIdentifier: String? = Bundle.main.bundleIdentifier,
        shortVersion: String? = Bundle.main
            .object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String,
        buildNumber: String? = Bundle.main
            .object(forInfoDictionaryKey: "CFBundleVersion") as? String
    ) -> BuildIdentity? {
        guard !isNonUserRuntime(arguments) else { return nil }
        guard let bundle = bundleIdentifier,
              bundle == "com.nyanako.tokenbar",
              let short = shortVersion,
              let build = buildNumber
        else { return nil }
        return BuildIdentity(bundleIdentifier: bundle, shortVersion: short, buildNumber: build)
    }
}

/// Off-main-actor write ordering for the reopen snapshot. `DashboardModel`
/// takes a monotonic capture sequence on the MAIN actor before handing off
/// to this actor, so a delayed older encode can never overwrite a newer
/// capture: state is keyed per directory (not a single global), so unrelated
/// callers — chiefly SelfTest fixtures, each with its own temporary
/// directory — cannot dedup or reorder against each other's writes.
actor SnapshotWriter {
    static let shared = SnapshotWriter()

    private struct State {
        var highestCompletedSequence = 0
        /// The complete encoded bytes of the last ACCEPTED write, with
        /// `savedAt` held at a fixed canonical value so only a real content
        /// change counts. Updated only after a successful rename — a skip or
        /// a failed write must never poison the next real change into being
        /// silently dropped.
        var lastWrittenCanonicalBytes: Data?
        /// When the file on disk was last stamped. Content dedup alone lets an
        /// idle machine keep a byte-identical payload until `maxAge` rejects
        /// it, restoring the blank startup this slice exists to remove — even
        /// though every live fetch in between succeeded. Freshness is a
        /// separate question from content identity.
        var lastWrittenAt: Date?
    }

    private var states: [String: State] = [:]

    private static let canonicalDate = Date(timeIntervalSince1970: 0)
    /// Re-stamp an unchanged snapshot no less often than this. Well inside
    /// `SnapshotStore.maxAge`, and at roughly 117 KB it costs one write a week
    /// on a machine with no new usage at all.
    static let renewalInterval: TimeInterval = 60 * 60 * 24 * 7
    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return encoder
    }()

    /// `sequence` was already fixed on the main actor before this call was
    /// even dispatched. The high-water mark advances on every outcome below —
    /// accepted write, digest skip, and failure alike — because leaving it
    /// unmoved on a skip or a failure is exactly the hole that let a stale,
    /// slower encode land after a newer one had already settled by either of
    /// those routes.
    func submit(sequence: Int, envelope: SnapshotEnvelope, directory: URL) async {
        let key = directory.path
        var state = states[key] ?? State()
        guard sequence > state.highestCompletedSequence else { return }
        state.highestCompletedSequence = sequence
        defer { states[key] = state }

        let canonical = SnapshotEnvelope(
            snapshotSchemaVersion: envelope.snapshotSchemaVersion,
            bundleIdentifier: envelope.bundleIdentifier,
            shortVersion: envelope.shortVersion,
            buildNumber: envelope.buildNumber,
            savedAt: Self.canonicalDate,
            selectedYear: envelope.selectedYear,
            payload: envelope.payload,
            knownYears: envelope.knownYears,
            scanRoots: envelope.scanRoots)
        guard let canonicalBytes = try? Self.encoder.encode(canonical) else { return }
        // Skip only when the content is unchanged AND the stamp is still fresh.
        // Dedup on content alone would let an idle machine keep the same bytes
        // past `maxAge`, at which point restore rejects a snapshot whose live
        // fetches had all succeeded.
        let stale = state.lastWrittenAt.map {
            envelope.savedAt.timeIntervalSince($0) >= Self.renewalInterval
        } ?? true
        guard canonicalBytes != state.lastWrittenCanonicalBytes || stale else { return }
        guard let bytes = try? Self.encoder.encode(envelope) else { return }
        guard SnapshotStore.write(bytes, in: directory) else { return }
        state.lastWrittenCanonicalBytes = canonicalBytes
        state.lastWrittenAt = envelope.savedAt
    }
}
