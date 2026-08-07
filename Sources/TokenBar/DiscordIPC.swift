import os
import Darwin
import Foundation

/// Transport for the (opt-in, default-off) Discord Rich Presence feature:
/// framing codec, wire serialization, and the socket lifecycle.
///
/// `AppDelegate` constructs the one production client and `SettingsPanel`
/// declares the opt-in switch; this file is live behind it. The
/// transport ships one milestone ahead of the wiring on purpose: everything
/// here is hermetically testable, while "can it leak" only becomes answerable
/// once there is a switch to flip.
///
/// `DiscordPresence.Payload.fields` is the payload-derived published surface.
/// This file may rename a key on the way out (Discord nests the asset key as
/// `assets.large_image`); it may not drop a field or alter a value.
///
/// It adds exactly four leaves of its own, and every one is independent of the
/// user: the pid, a bare UUID nonce, and the label and URL of a single button
/// linking to this project's repository. The button is here rather than in
/// `fields` on purpose — a value inside `fields` is admitted by the wire
/// assertion's own expected value, and a compile-time constant has no input for
/// the value scans to poison, so a URL that later grew a query parameter would
/// pass every payload check. Carried here it is pinned by literal assertions
/// instead. Adding a FIFTH leaf, or making any of these four depend on the
/// machine, is the thing this sentence exists to forbid.
///
/// `leafStrings(_:)` exists so that contract is asserted against the actual
/// serialized bytes rather than a hand-maintained list.
enum DiscordIPC {
    /// Public constant, not a secret. A Discord application id is visible to
    /// anyone who sees the presence; the client secret and bot token this
    /// feature does not need must never enter the repo or the bundle.
    static let applicationID = "1534085299163107348"

    /// Discord does not document a frame length limit, so this is ours. 64 KiB
    /// is ~50x the largest frame this client can provoke (a READY carrying a
    /// user object is well under 1 KiB) and small enough that a hostile
    /// endpoint cannot make us allocate anything interesting. The bound is
    /// checked before any allocation sized from the wire.
    static let maxFrameLength: UInt32 = 64 * 1024

    /// The repository link the presence carries. Discord allows at most two
    /// buttons, each `{label, url}`, with the label 1–32 characters and the URL
    /// 1–512.
    ///
    /// **Transport-layer constants, deliberately not payload fields.** Putting
    /// the URL in `Payload.fields` would open a channel the privacy assertions
    /// structurally cannot see: the wire assertion's expected value *is*
    /// `fields.values` plus the envelope constants, so anything placed there is
    /// admitted by definition; and the value scans work by poisoning the
    /// payload's inputs, which a compile-time constant does not have. A later
    /// `buttonURL + "?ref=" + installID` would pass every one of them. It would
    /// also break the "no path-like segment in the payload" assertion outright,
    /// and the natural exemption for that is exactly what would admit the query
    /// parameter.
    ///
    /// Carried here instead, alongside the pid and the nonce — the other two
    /// leaves the transport synthesises — these are pinned by literal
    /// assertions, so a non-constant value fails the strongest check in the
    /// suite with no new machinery.
    ///
    /// Nothing about the user is in either value, and nothing may be added to
    /// them. A query parameter, a fragment, or anything derived from the
    /// machine belongs to a different feature and a different review.
    ///
    /// **What enforces that, and what does not.** The wire assertions check the
    /// URL that actually reaches the socket: bare host, no query, no fragment,
    /// pinned to the literal. They run under `swift build` as a bare
    /// executable, and the app ships from `make bundle` as a release `.app`.
    /// A value keyed on that difference — `Bundle.main.bundleIdentifier` being
    /// nil under test and set in the bundle is the obvious key — is literal
    /// where the suite looks and something else where it ships.
    ///
    /// Three source scans were written against that and all three were
    /// escaped: pinning the emitted value missed a conditional constant,
    /// pinning the declaration missed a suffix applied at the use site, and
    /// pinning the use site missed both a later mutation of the same dictionary
    /// and a rewrite inside `serialize`. The last also failed when a local was
    /// renamed, which is the shape of a guard that gets edited rather than
    /// obeyed.
    ///
    /// The real gap is that the suite does not observe the configuration that
    /// ships, and no source scan closes it — running the suite from the bundled
    /// binary would. Until that exists this comment is the enforcement: the two
    /// constants are literals, and the frame is built from them and nothing
    /// else. The same exposure has always applied to `pid()` and `nonce()`.
    static let buttonLabel = "View on GitHub"
    static let buttonURL = "https://github.com/Nanako0129/TokenBar"

    /// How a publish's own visibility change relates to what may be published.
    /// Three states, not a Bool: see `DiscordIPCClient.publish(_:visibility:)`
    /// for why the middle one — an ordinary sample that changed nothing — has
    /// to be distinguishable from an unhide.
    /// Two independent effects, which is why this is a struct and not an enum.
    /// A single coalesced turn can both replace what is published and put
    /// information back, and those want opposite things from the grant: the
    /// replacement must retire the old payload, while the addition must clear
    /// any bypass so it cannot go out inside the floor. An enum forced one of
    /// them to be dropped, and dropping the clear is the privacy failure.
    struct VisibilityChange: Equatable {
        /// What this change does to the one-shot permission to skip the
        /// publish floor.
        enum Grant: Equatable {
            /// The user removed content: skip the floor, it is owed to them.
            case arm
            /// The user put content back: it must not ride an unspent grant.
            case clear
            /// Neither claim. An ordinary sample inherits whatever is pending.
            case leave
        }

        /// Whether earlier queued work was computed against a state that no
        /// longer holds and must not reach the socket.
        var retires: Bool
        var grant: Grant

        /// An ordinary sample. Whatever the current hidden set is, this payload
        /// was built from it.
        static let none = VisibilityChange(retires: false, grant: .leave)
        /// The user hid something. Must not queue behind the publish floor.
        static let reducing = VisibilityChange(retires: true, grant: .arm)
        /// The user unhid something, putting information back on the profile.
        static let increasing = VisibilityChange(retires: false, grant: .clear)
        /// The published content was replaced rather than narrowed or widened —
        /// a change of which agent is published. It retires earlier work, since
        /// those payloads were built for a different client, but earns no grant
        /// of its own: hide and unhide alternate so a reduction's bypass is
        /// self-limiting, while A→B→C→A does not, and four bypasses is a full
        /// per-client breakdown pushed out in one second.
        static let retiring = VisibilityChange(retires: true, grant: .leave)

        /// Two preference changes landing in one coalesced turn. The retire is
        /// the union — losing one lets a payload built against a state that no
        /// longer holds reach the socket — and the grant takes the strongest
        /// claim, `arm` over `clear` over `leave`, for the same reason a single
        /// write that both hides and unhides is a reduction: the content the
        /// user removed outranks the sampling rate.
        func combined(with other: VisibilityChange) -> VisibilityChange {
            let strongest: Grant
            if grant == .arm || other.grant == .arm {
                strongest = .arm
            } else if grant == .clear || other.grant == .clear {
                strongest = .clear
            } else {
                strongest = .leave
            }
            return VisibilityChange(retires: retires || other.retires, grant: strongest)
        }
    }

    enum Opcode: UInt32 {
        case handshake = 0, frame = 1, close = 2, ping = 3, pong = 4
    }

    enum Failure: Error {
        /// Deliberately the only case: this error is not logged, and a richer
        /// one would only invite putting a path or an errno somewhere.
        case unavailable
    }

    // MARK: - Framing

    /// 4-byte little-endian opcode, 4-byte little-endian length, then `body`.
    static func encode(_ op: Opcode, _ body: Data) -> Data {
        var out = Data(capacity: 8 + body.count)
        withUnsafeBytes(of: op.rawValue.littleEndian) { out.append(contentsOf: $0) }
        withUnsafeBytes(of: UInt32(body.count).littleEndian) { out.append(contentsOf: $0) }
        out.append(body)
        return out
    }

    enum DecodeResult: Equatable {
        /// Not a whole frame yet; `buffer` is untouched.
        case needMore
        /// A complete frame with a known opcode and a parseable JSON body.
        case frame(Opcode, Data)
        /// Consumed and dropped: unknown opcode, or a body that is not valid
        /// UTF-8 JSON. Silent by design — logging the content of a frame from
        /// an endpoint we do not control is the thing we are avoiding.
        case discard
        /// The stream is not trustworthy; the caller must disconnect.
        case fatal
    }

    /// Consume one frame from the front of `buffer`.
    static func decode(from buffer: inout Data) -> DecodeResult {
        guard buffer.count >= 8 else { return .needMore }
        let base = buffer.startIndex
        let rawOpcode = readLE32(buffer, at: base)
        let length = readLE32(buffer, at: base + 4)
        // Before the buffer-completeness check, and before any allocation: a
        // 4 GiB length must be refused outright, not waited on.
        guard length <= maxFrameLength else { return .fatal }
        let total = 8 + Int(length)
        guard buffer.count >= total else { return .needMore }
        let body = Data(buffer[(base + 8)..<(base + total)])
        buffer.removeFirst(total)
        guard let op = Opcode(rawValue: rawOpcode) else { return .discard }
        // One validity check covers both malformed JSON and malformed UTF-8:
        // JSONSerialization rejects a body that is not valid UTF-8 as well.
        // A separate encoding check would be unreachable code pretending to be
        // a second guard.
        guard (try? JSONSerialization.jsonObject(with: body)) != nil else { return .discard }
        return .frame(op, body)
    }

    private static func readLE32(_ data: Data, at index: Data.Index) -> UInt32 {
        UInt32(data[index])
            | UInt32(data[index + 1]) << 8
            | UInt32(data[index + 2]) << 16
            | UInt32(data[index + 3]) << 24
    }

    // MARK: - Serialization

    static func handshakeJSON() -> Data {
        serialize(["v": 1, "client_id": applicationID])
    }

    /// Map the user-derived surface onto Discord's activity wire shape, and
    /// add the transport's own constant leaves alongside it. `nil`
    /// clears the activity (`"activity":null`), which is what `stop()` sends
    /// before closing the socket.
    static func activityJSON(
        _ payload: DiscordPresence.Payload?, pid: Int32, nonce: String
    ) -> Data {
        var activity: Any = NSNull()
        if let payload {
            var fields: [String: Any] = [:]
            for (key, value) in payload.fields {
                // Renaming is allowed, adding is not. An unrecognized key goes
                // out under its own name rather than being dropped: the
                // contract says the transport must not drop a published field.
                if key == "largeImageKey" {
                    fields["assets"] = ["large_image": value]
                } else {
                    fields[key] = value
                }
            }
            // Added by the transport, like the pid and the nonce, and only
            // alongside a real activity: a clear is `nil` and carries nothing,
            // buttons included. This is the one place `fields` is added to, and
            // it adds constants rather than anything derived from the payload.
            fields["buttons"] = [["label": buttonLabel, "url": buttonURL]]
            activity = fields
        }
        return serialize([
            "cmd": "SET_ACTIVITY",
            "args": ["pid": Int(pid), "activity": activity],
            "nonce": nonce,
        ])
    }

    /// Every scalar leaf of a serialized frame, rendered as a string — numbers
    /// and booleans included, so that a field smuggled in as a number (a
    /// `startTimestamp`, a pid, a hostname hash) is just as visible to the
    /// privacy assertions as a string would be. Keys are not leaves, which is
    /// what keeps renaming a field legal — see `leafKeys` for the channel that
    /// costs, and why "adding is visible" is only true with both.
    ///
    /// It walks the real JSON, including nesting. A hand-written list of what
    /// "should" be in there asserts the author's memory, not the bytes.
    static func leafStrings(_ json: Data) -> [String] {
        guard let root = try? JSONSerialization.jsonObject(
            with: json, options: [.fragmentsAllowed]) else { return [] }
        var out: [String] = []
        func walk(_ any: Any) {
            switch any {
            case let dict as [String: Any]:
                for key in dict.keys.sorted() { walk(dict[key]!) }
            case let array as [Any]:
                for element in array { walk(element) }
            case let string as String:
                out.append(string)
            case is NSNull:
                out.append("null")
            default:
                out.append(String(describing: any))
            }
        }
        walk(root)
        return out
    }

    /// Every object key in a serialized frame, nesting included.
    ///
    /// `leafStrings` ignores keys on purpose, and that leaves the key itself as
    /// an unwatched channel: a frame carrying `"x-<hostname>": [:]` adds no
    /// leaf (an empty object has none) and changes no count at the levels that
    /// are counted, so every value-side assertion stays green while the
    /// hostname ships. Pinning the key set is what closes it, which is why
    /// both functions exist rather than one.
    static func leafKeys(_ json: Data) -> [String] {
        guard let root = try? JSONSerialization.jsonObject(
            with: json, options: [.fragmentsAllowed]) else { return [] }
        var out: [String] = []
        func walk(_ any: Any) {
            switch any {
            case let dict as [String: Any]:
                for key in dict.keys.sorted() {
                    out.append(key)
                    walk(dict[key]!)
                }
            case let array as [Any]:
                for element in array { walk(element) }
            default:
                break
            }
        }
        walk(root)
        return out
    }

    static let readyEvent = "ready"

    /// The only thing ever read out of an inbound frame, and the only string it
    /// can produce. A READY frame carries the Discord account's username, id
    /// and avatar; none of it may enter this process's state, its logs, or
    /// anything persisted. Returning a fixed token rather than the parsed
    /// object is the mechanism, not a stylistic choice.
    static func inbound(_ body: Data) -> String {
        guard let object = try? JSONSerialization.jsonObject(with: body) as? [String: Any],
              object["evt"] as? String == "READY" else { return "other" }
        return readyEvent
    }

    private static func serialize(_ object: [String: Any]) -> Data {
        (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
    }

    // MARK: - Socket

    /// `nil` when the path does not fit in `sun_path`. Truncation must never be
    /// the fallback: a truncated `sun_path` does not fail, it connects to a
    /// *different* path.
    static func unixSocketAddress(path: String) -> sockaddr_un? {
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let bytes = Array(path.utf8)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard bytes.count < capacity else { return nil }
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
        return addr
    }

    /// Only `discord-ipc-0`. Probing `discord-ipc-1..9` would widen the set of
    /// endpoints a same-user process can squat on for no user-visible gain.
    /// A socket that is close-on-exec from birth.
    ///
    /// Darwin has no `SOCK_CLOEXEC`, so this is the closest to atomic the
    /// platform allows — and it matters that the `fcntl` is here rather than
    /// after `connect()` returns. The app's Rust core spawns helpers
    /// (`/usr/bin/security`, a login shell, `claude --version`), and every
    /// instruction between `socket()` and the flag being set is a window in
    /// which one of those execs inherits the descriptor and keeps Discord
    /// seeing this connection long after the app has torn it down.
    static func makeSocket() -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return -1 }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        return fd
    }

    /// No deadline machinery here on purpose. A blocking `connect()` to a Unix
    /// socket cannot park the caller on Darwin: measured against a listener
    /// with `listen(fd, 1)` that never accepts, the first connect succeeds and
    /// every subsequent one returns `ECONNREFUSED` in under a millisecond.
    /// There is no round trip to wait on. Non-blocking connect plus `poll`
    /// would be machinery for a state this platform does not produce, and its
    /// only possible assertion — that the call returns quickly — passes
    /// whether or not the machinery is there.
    static func connectToDiscord() throws -> Int32 {
        guard let dir = ProcessInfo.processInfo.environment["TMPDIR"] else {
            throw Failure.unavailable
        }
        let path = (dir as NSString).appendingPathComponent("discord-ipc-0")
        guard var addr = unixSocketAddress(path: path) else { throw Failure.unavailable }
        let fd = makeSocket()
        guard fd >= 0 else { throw Failure.unavailable }
        let result = withUnsafePointer(to: &addr) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard result == 0 else {
            close(fd)
            throw Failure.unavailable
        }
        return fd
    }
}

/// Owns one Discord IPC connection: handshake, throttled publishing, bounded
/// reconnection, and a single kill switch.
///
/// All mutable state lives on `queue`, a serial utility queue — hence
/// `@unchecked Sendable`. Socket I/O must never run on the main thread or share
/// the tray's refresh Task.
final class DiscordIPCClient: @unchecked Sendable {
    /// Matches the reference implementation's `UPDATE_MIN_INTERVAL_MS`. Also a
    /// privacy floor: sampling frequency is what turns a presence into a
    /// working-hours trace, so this is not a performance knob to tune down.
    static let minPublishInterval: TimeInterval = 15
    /// `RECONNECT_DELAY_MS` in the reference implementation. Discord not
    /// running is the normal case, not an error worth retrying quickly.
    static let reconnectDelaySeconds: TimeInterval = 30
    /// Bounded on purpose: after ~2.5 minutes of CONSECUTIVE failures, stop. A
    /// presence is not worth a background loop that outlives the user's
    /// interest in it. The counter resets once a connection reaches READY, so
    /// a long session that watches Discord restart six times still reconnects.
    /// Without any reset the budget was per `start()` lifetime, which the user
    /// would experience as the presence silently never coming back; resetting
    /// on the socket open instead would reconnect forever against a peer that
    /// accepts and immediately drops.
    static let maxReconnectAttempts = 5

    /// A peer that accepts the socket and then says nothing must not hold the
    /// client connected-but-never-ready forever. `SO_RCVTIMEO` cannot cover
    /// this: the read source only calls `readAvailable()` when bytes arrive,
    /// so a silent endpoint never triggers a `recv()` and the socket timeout
    /// is never observed. Discord answers a handshake in milliseconds.
    static let readyTimeoutSeconds: TimeInterval = 10

    /// Instance-level so the selftest can shrink it. Production never assigns.
    var reconnectDelay: TimeInterval = DiscordIPCClient.reconnectDelaySeconds
    /// Instance-level so the selftest can assert on the bytes that reach the
    /// socket rather than on an internal flag. **Production never assigns**,
    /// and this is not a knob: the static it defaults to is a privacy floor,
    /// because publish frequency is what turns a presence into a working-hours
    /// trace. Lowering it in shipping code is a privacy regression, not tuning.
    var publishInterval: TimeInterval = DiscordIPCClient.minPublishInterval
    /// Instance-level for the same reason.
    var readyTimeout: TimeInterval = DiscordIPCClient.readyTimeoutSeconds

    /// Whether the feature is on, and *which grant of it* the caller was under.
    ///
    /// Set off-queue and read off-queue, because `stop()` flipping `running`
    /// from inside a queued block lets any publish enqueued a moment earlier
    /// run first, still see `running == true`, and put one more activity out
    /// after the user withdrew consent — which the clear that follows cannot
    /// take back. This is the one piece of state that cannot live on the queue.
    ///
    /// An epoch and not just a Bool, because a Bool cannot tell "consent is
    /// granted now" from "this work was enqueued under a consent that has since
    /// been withdrawn". Switching the feature off and back on while a publish
    /// is still queued — the serial queue sitting in socket I/O is enough —
    /// would otherwise let the later `start()` re-authorize a payload computed
    /// before the withdrawal, and that payload may name a client the user hid
    /// in between. `stop()` bumps the epoch; a publish captures it at call time
    /// and is refused if it no longer matches, so no later `start()` can
    /// re-authorize work from before the withdrawal.
    ///
    /// Two things bump, and they are the two ways outstanding work stops being
    /// valid: `stop()`, because the user withdrew consent, and a `.reducing`
    /// publish, because everything computed before it was computed against a
    /// larger visible set. Both retire what came before; only the first also
    /// clears `granted`.
    ///
    /// `start()` restores `granted` and deliberately leaves the epoch alone: a
    /// publish made while the retry budget was spent is the intent a later
    /// `start()` is supposed to restore, and bumping there would refuse exactly
    /// that payload. That reasoning does not extend to the `.reducing` bump —
    /// which happens in `publish` itself and takes the bumped value as its own
    /// ticket, so it retires its predecessors without retiring itself.
    private struct Consent {
        var granted: Bool
        var epoch: UInt64
        /// The one-shot permission to skip the publish floor.
        ///
        /// It lives HERE, under the lock, rather than as queue-owned state,
        /// and that placement is the whole design. An earlier revision armed it
        /// inside the queued block while every decision about it was taken
        /// off-queue, which opened a window between "a reduction was requested"
        /// and "the grant exists" — and five review rounds found four distinct
        /// defects living in that window: a retire retiring the block before it
        /// could arm, a withdrawal abandoning it, an older block clearing a
        /// newer epoch's grant, and a rejected clear being read as no grant.
        ///
        /// Set at publish time under the same lock as the epoch, there is no
        /// window, no ownership question, and nothing for a later publish to
        /// inherit or destroy: whatever the last caller asked for is simply
        /// what is true.
        var floorBypass = false
    }
    private let consentLock = OSAllocatedUnfairLock(
        initialState: Consent(granted: true, epoch: 0))
    /// The epoch `pending` was recorded under. `flush()` is reached from the
    /// READY restore and the throttle's deferred wake-up as well as from
    /// `publish`, and those carry no ticket of their own.
    private var pendingEpoch: UInt64 = 0

    /// Off-queue read, so a queued block asks about the state as it is *now*.
    private func consentAllows(_ epoch: UInt64) -> Bool {
        consentLock.withLock { $0.granted && $0.epoch == epoch }
    }

    /// Whether the feature is on right now, with no ticket. Opening a socket is
    /// not authorized by a past grant the way a payload is: what matters is
    /// only whether the user wants this connected at the moment it would be
    /// created, so an off-and-on-again reconnects rather than being refused.
    private func consentGranted() -> Bool {
        consentLock.withLock { $0.granted }
    }

    private let connectFD: @Sendable () throws -> Int32
    private let queue = DispatchQueue(label: "com.nyanako.tokenbar.discord-ipc", qos: .utility)

    private var fd: Int32 = -1
    /// The user's intent: `start()` sets it, `stop()` clears it. Deliberately
    /// NOT cleared by `giveUp()` — a spent retry budget is a connection state,
    /// not the user changing their mind, and conflating the two meant producer
    /// updates were dropped while the client sat abandoned.
    private var running = false
    /// Connection state: retries are spent and nothing is scheduled. A later
    /// `start()` clears it and tries again.
    private var abandoned = false
    private var ready = false
    private var buffer = Data()
    private var source: DispatchSourceRead?
    private var reconnectWork: DispatchWorkItem?
    private var throttleWork: DispatchWorkItem?
    private var readyWork: DispatchWorkItem?
    private var attempts = 0
    private var lastSent: DispatchTime?
    private var pending: DiscordPresence.Payload?
    /// The last payload actually sampled from the producer, across
    /// connections. `flush` compares against it to tell a fresh sample from a
    /// restore of bytes Discord already had.
    private var lastSampledPayload: DiscordPresence.Payload?
    /// What the CURRENT connection has been given. Reset by `teardown`, which
    /// is what makes a restore after a reconnect distinguishable from a
    /// duplicate publish on a connection that already holds it.
    ///
    /// Its own type, not `Payload?`, because `nil` is a payload here: it is the
    /// clear. Using one `nil` for both "nothing delivered yet" and "a clear was
    /// delivered" made a fresh connection claim it already held the clear, so a
    /// clear that lost its socket mid-send was dropped on the reconnect instead
    /// of retried — leaving the activity of clients the user had just hidden on
    /// their profile until some later publish happened to remove it.
    private enum Delivered: Equatable {
        case nothing
        case payload(DiscordPresence.Payload?)
    }
    private var deliveredOnThisConnection: Delivered = .nothing
    private var hasPending = false
    /// One-shot permission for the *next* write to skip the publish floor,
    /// granted by a `privacyReducing` publish and spent on that write. Separate
    /// from `lastSent` on purpose: the bypass is about one update, the clock is
    /// about the sampling rate, and collapsing the two lets a clear leave the
    /// rate unbounded. See `publish(_:privacyReducing:)`.

    private var inboundToken = ""
    private var writeErrno: Int32 = 0

    init(connect: @escaping @Sendable () throws -> Int32) {
        self.connectFD = connect
    }

    // MARK: - Public surface

    /// Idempotent: a second call while running is a no-op, not a second socket.
    func start() {
        // Outside the block for the same reason `stop()`'s flip is, and the two
        // only compose if BOTH are: the sole production call site is
        // `applyDiscordPresence`'s back-to-back `start()` + `publish()`, so
        // every window in which a publish sits queued has a start block queued
        // ahead of it. Re-arming from inside that block re-arms *after* a
        // `stop()` that has already run off-queue, and the publish behind it
        // then flushes with consent nominally restored — which is exactly the
        // frame `stop()` exists to prevent. Off-queue, the two flags are
        // written in call order, so the later `stop()` wins.
        consentLock.withLock { $0.granted = true }
        queue.async {
            // Idempotent while live, but an abandoned client must be able to
            // try again — that is the whole point of not calling `stop()` when
            // the budget runs out.
            guard !self.running || self.abandoned else { return }
            self.running = true
            self.abandoned = false
            self.attempts = 0
            self.openConnection()
        }
    }

    /// The single kill switch: cancel every scheduled wake-up, clear the
    /// activity on Discord's side, then close the socket. Order matters — the
    /// clear cannot be sent through a closed socket.
    func stop() {
        // Before the block, not inside it: everything already queued has to see
        // this, and queued work is exactly what the block cannot reach back to.
        // The epoch bump is what a later `start()` cannot undo.
        // The grant goes with it. A hide that never reached the socket is
        // withdrawn along with consent, so carrying its bypass into a later
        // re-enable would let the next payload skip the floor on a promise
        // nobody is owed any more.
        consentLock.withLock {
            $0.granted = false
            $0.epoch &+= 1
            $0.floorBypass = false
        }
        queue.async {
            let wasRunning = self.running
            self.running = false
            self.abandoned = false
            self.reconnectWork?.cancel()
            self.throttleWork?.cancel()
            self.readyWork?.cancel()
            self.readyWork = nil
            // Cleared whether or not it was running. `giveUp()` deliberately
            // leaves `pending` behind so a later `start()` can restore, and a
            // `stop()` that bailed on `!running` would leave that payload
            // alive to be republished after the user turned the feature off.
            self.hasPending = false
            self.pending = nil
            // The grant is NOT cleared here. It is cleared off-queue in
            // `stop()` itself, at the moment consent is withdrawn, because this
            // block runs later: a `start()` and a reducing `publish()` can both
            // land in between, and clearing here would take that new grant
            // instead of the withdrawn one.
            self.lastSampledPayload = nil
            self.deliveredOnThisConnection = .nothing
            if wasRunning, self.fd >= 0 {
                self.writeFrame(
                    .frame,
                    DiscordIPC.activityJSON(nil, pid: self.pid(), nonce: self.nonce()))
            }
            self.teardown()
        }
    }

    /// `nil` clears the activity. Coalescing, not queueing: only the newest
    /// payload is ever published.
    ///
    /// `visibility` is how the user's own action changed what may be published,
    /// and it has three states rather than two because coalescing makes the
    /// missing one matter. A `.reducing` publish may still be sitting unwritten
    /// — the connection is reconnecting, or has not finished its handshake —
    /// when the next publish overwrites `pending`. What that later payload
    /// deserves depends on what the user did, not on the fact that it arrived:
    ///
    ///   - `.none` (an ordinary sample) still carries the reduction, because
    ///     the payload is rebuilt from the current hidden set every time. Its
    ///     bypass is inherited on purpose. Dropping it here would throttle the
    ///     reduction itself and leave a client the user hid on a public profile
    ///     for the rest of the floor.
    ///   - `.increasing` (the user unhid something) puts information back, so
    ///     it takes the bypass with it. Inheriting it would let an unhide
    ///     publish sub-floor, which is a sample of the user's activity at a
    ///     higher rate than the floor promises.
    ///
    /// A write that hides one client and unhides another is `.reducing`: the
    /// content the user removed outranks the sampling rate.
    func publish(
        _ payload: DiscordPresence.Payload?,
        visibility: DiscordIPC.VisibilityChange = .none
    ) {
        // Captured here, off-queue, because the point is which grant the CALLER
        // was under — not which one happens to be current by the time the queue
        // reaches this work.
        //
        // A reduction also *retires* everything computed before it. Those
        // payloads were built against a larger visible set, so one of them
        // reaching the socket first puts the client the user just hid back on
        // the profile: for microseconds if the reduction follows immediately,
        // for a whole reconnect if the socket dies in between.
        //
        // This is the producer-side half of the same withdrawal the epoch
        // already protects on the consumer side, and it is deliberately keyed
        // on the reduction rather than on the switch going off. The defaults
        // observer coalesces, so an off-then-on pair can collapse before
        // `AppDelegate` ever sees the `false` — but the hide inside it always
        // surfaces as `.reducing`. Retiring stale work on a fact that is always
        // observable beats retiring it on a transition that sometimes is not.
        // `inheritsReduction` carries a hide's grant across a retire. The
        // retire's own payload already contains the new hidden set, so the
        // grant is still owed even though the block that would have armed it
        // has been retired.
        let ticket = consentLock.withLock { state -> UInt64 in
            if visibility.retires { state.epoch &+= 1 }
            switch visibility.grant {
            case .arm: state.floorBypass = true
            // An unhide puts information back and must not ride a grant.
            case .clear: state.floorBypass = false
            // A retire earns nothing of its own and destroys nothing either.
            case .leave: break
            }
            return state.epoch
        }
        queue.async {
            // Recorded even while abandoned: the producer's latest intent is
            // what a later `start()` should restore, not whatever happened to
            // be current when the retries ran out.
            guard self.running, self.consentAllows(ticket) else { return }
            self.pending = payload
            self.pendingEpoch = ticket
            self.hasPending = true
            // A one-shot permission to skip the floor, NOT a reset of its
            // clock. Clearing `lastSent` here looked equivalent and was not:
            // hiding every visible client publishes a *clear*, which carries no
            // new information and therefore does not re-arm the clock on its
            // way out, so the cleared `lastSent` survived and the next real
            // payload — an unhide a second later — went out with no floor at
            // all. That is a sub-15s sample of the user's activity, which is
            // the one thing the floor exists to prevent.
            self.flush()
        }
    }

    // MARK: - Selftest seams
    //
    // Internal, not `#if DEBUG`: a symbol that exists only in one configuration
    // is a symbol that breaks the release build the first time someone calls it
    // without the guard.

    /// Blocks until everything already queued has run.
    func drainForTesting() { queue.sync {} }

    var isConnectedForTesting: Bool { queue.sync { fd >= 0 } }
    var inboundTokenForTesting: String { queue.sync { inboundToken } }
    var writeErrnoForTesting: Int32 { queue.sync { writeErrno } }
    var reconnectPendingForTesting: Bool {
        queue.sync { reconnectWork.map { !$0.isCancelled } ?? false }
    }

    /// Runs `breakPeer` and writes on the *same* queue item, so the read
    /// source's EOF handler cannot tear the connection down in between. That
    /// makes "write to a socket whose peer is gone" — the SIGPIPE case —
    /// deterministic instead of a race.
    ///
    /// Takes a closure rather than a descriptor: an earlier signature accepted
    /// an `Int32` and closed it, which is a `close()` on whatever number the
    /// caller passed. Ordering is the only thing this seam needs to own.
    func probeWriteForTesting(after breakPeer: @Sendable () -> Void) {
        queue.sync {
            breakPeer()
            writeFrame(.frame, DiscordIPC.activityJSON(nil, pid: pid(), nonce: nonce()))
        }
    }

    /// Holds the serial queue until `gate` is signalled, so a test can enqueue
    /// work that is *guaranteed* to still be waiting when it calls `stop()`
    /// from another thread. That ordering is the whole subject of the consent
    /// assertion — without a way to pin it, "a publish already queued when the
    /// switch went off never reaches the socket" is a race that passes whether
    /// or not the flag is flipped off-queue.
    func holdQueueForTesting(until gate: DispatchSemaphore) {
        queue.async { gate.wait() }
    }

    /// Drives the reconnect path directly so the "stopped means stopped"
    /// assertion does not have to wait for a real disconnect it can no longer
    /// provoke (after `stop()` there is no socket left to break).
    func scheduleReconnectForTesting() {
        queue.async { self.scheduleReconnect() }
    }

    // MARK: - Connection

    private func openConnection() {
        // The one place a connection can come into existence, and therefore the
        // one place that has to honour "after stop(), no path reconnects".
        //
        // There is deliberately no second `fd < 0` *guard* here: `start()`'s
        // `!running` guard is what keeps a live connection from being replaced,
        // and two guards for one invariant means neither can be shown to fail
        // on its own.
        //
        // Consent is read here rather than at the two call sites, and read
        // off-queue, because `running` only says what this queue believed when
        // the work was enqueued. Switching the feature on and then off while
        // the queue is busy leaves a start block — or a reconnect whose
        // deadline fired first — queued ahead of `stop()`, and it would open a
        // socket and hand Discord a handshake after the user opted out. Nothing
        // of the user's usage goes out, because the publish behind it is
        // epoch-gated, but the gate's own contract is that this process may not
        // connect at all. Current state, not the enqueued state: an off and
        // then on again is consent, and it should connect.
        guard running, consentGranted() else { return }
        // Not a guard — a precondition made true. Reaching here with a live fd
        // would overwrite `fd` and `source` without cancelling the old source,
        // leaking the descriptor while libdispatch kept firing on it. That was
        // reachable from `scheduleReconnectForTesting`, and would be reachable
        // from any future caller too, so it is closed here at the one place a
        // connection is born rather than at each call site.
        if fd >= 0 { teardown() }
        guard let newFD = try? connectFD() else {
            scheduleReconnect()
            return
        }
        // Before anything else on this descriptor. The app's Rust core spawns
        // helpers (`/usr/bin/security`, a login shell, `claude --version`), and
        // a child that inherits this socket keeps Discord seeing the connection
        // open after we have torn it down — stale presence outliving the app's
        // own teardown. Applied here rather than in `connectToDiscord` so it
        // covers every descriptor the client adopts, injected ones included.
        _ = fcntl(newFD, F_SETFD, FD_CLOEXEC)
        // Immediately, before any write can happen: Darwin's default for a
        // write to a closed socket is SIGPIPE, which kills the whole app, and
        // Discord quitting mid-session is ordinary.
        //
        // The return value is checked because this call really can fail — it
        // returns EINVAL on a socket whose peer has already gone away, which is
        // reachable if Discord dies between `connect()` and here. A socket we
        // could not make SIGPIPE-safe is not one we are willing to write to, so
        // it is dropped rather than used.
        var on: Int32 = 1
        guard setsockopt(
            newFD, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size)) == 0
        else {
            close(newFD)
            scheduleReconnect()
            return
        }
        // Backstop against a peer that opens the socket and then says nothing.
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        setsockopt(newFD, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(newFD, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        fd = newFD
        buffer.removeAll()
        ready = false

        let readSource = DispatchSource.makeReadSource(fileDescriptor: newFD, queue: queue)
        // Bound to `newFD`, not just to `self`. A source that has already
        // queued a readability event can run after this client has torn down
        // and reconnected, and an unbound handler would then read from the
        // *new* socket before its own source has fired — blocking the serial
        // queue on a descriptor with nothing to give.
        readSource.setEventHandler { [weak self] in
            guard let self, self.fd == newFD else { return }
            self.readAvailable()
        }
        // Closing in the cancel handler, not beside `cancel()`: the source may
        // still be about to run its event handler on this fd.
        readSource.setCancelHandler { close(newFD) }
        source = readSource
        readSource.resume()

        writeFrame(.handshake, DiscordIPC.handshakeJSON())
        armReadyDeadline()
    }

    /// An endpoint that accepts the socket and then stays silent would
    /// otherwise hold the client connected-but-never-ready forever, with every
    /// publish parked behind `ready`. `SO_RCVTIMEO` does not cover it: the read
    /// source only fires when bytes arrive, so no `recv()` ever runs on an idle
    /// socket and its timeout is never observed.
    private func armReadyDeadline() {
        readyWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.running, !self.ready, self.fd >= 0 else { return }
            self.handleDisconnect()
        }
        readyWork = work
        queue.asyncAfter(deadline: .now() + readyTimeout, execute: work)
    }

    private func scheduleReconnect() {
        guard attempts < Self.maxReconnectAttempts else {
            // The budget is spent, so go back to the stopped state instead of
            // sitting in `running` with nothing scheduled. Leaving it true
            // made a later `start()` hit the idempotence guard and silently do
            // nothing, which turned a bounded retry into a permanent dead
            // client for anything started once at launch. `pending` is kept:
            // a later `start()` should republish what was last intended.
            giveUp()
            return
        }
        attempts += 1
        let work = DispatchWorkItem { [weak self] in self?.openConnection() }
        reconnectWork = work
        queue.asyncAfter(deadline: .now() + reconnectDelay, execute: work)
    }

    /// Not `stop()`: there is no socket left to send a clear on, and this is
    /// not the user asking to stop. It only returns the client to a state a
    /// later `start()` can act on.
    private func giveUp() {
        abandoned = true
        attempts = 0
        throttleWork?.cancel()
        throttleWork = nil
        reconnectWork?.cancel()
        reconnectWork = nil
        teardown()
    }

    private func teardown() {
        ready = false
        // A replacement connection holds nothing yet.
        deliveredOnThisConnection = .nothing
        readyWork?.cancel()
        readyWork = nil
        buffer.removeAll()
        fd = -1
        if let source {
            source.cancel()
            self.source = nil
        }
    }

    private func handleDisconnect() {
        teardown()
        if running { scheduleReconnect() }
    }

    // MARK: - Reading

    private func readAvailable() {
        guard fd >= 0 else { return }
        var chunk = [UInt8](repeating: 0, count: 4096)
        let count = recv(fd, &chunk, chunk.count, 0)
        if count == 0 { handleDisconnect(); return }
        if count < 0 {
            if errno == EINTR || errno == EAGAIN { return }
            handleDisconnect()
            return
        }
        buffer.append(contentsOf: chunk[0..<count])
        while true {
            switch DiscordIPC.decode(from: &buffer) {
            case .needMore:
                return
            case .discard:
                continue
            case .fatal:
                handleDisconnect()
                return
            case .frame(let op, let body):
                switch op {
                case .ping:
                    // The protocol wants the ping's own body echoed back. These
                    // are the peer's bytes going to the peer that sent them:
                    // nothing local is added, nothing is retained, and the
                    // length is already bounded by `maxFrameLength`. Do not
                    // "improve" this into something that reads or logs `body`.
                    //
                    // Consent-gated like every other write, so the invariant
                    // holds without exceptions: after a withdrawal, the only
                    // thing this process sends Discord is the clear. A ping
                    // that arrived before the user opted out can still have its
                    // read handler queued ahead of `stop()`'s block, and the
                    // socket is closed moments later regardless — answering it
                    // buys nothing and costs the one sentence that makes the
                    // rule checkable.
                    if consentGranted() { writeFrame(.pong, body) }
                case .close:
                    handleDisconnect()
                    return
                default:
                    inboundToken = DiscordIPC.inbound(body)
                    if inboundToken == DiscordIPC.readyEvent {
                        ready = true
                        // The retry budget resets HERE, not when `connect()`
                        // returns. A socket that opens and dies before the
                        // handshake — Discord running but refusing us — is
                        // still a failed attempt; resetting on the open would
                        // let that case reconnect at `reconnectDelay` forever,
                        // which is the unbounded background loop
                        // `maxReconnectAttempts` exists to prevent.
                        attempts = 0
                        readyWork?.cancel()
                        readyWork = nil
                        // A replacement connection starts with no activity on
                        // Discord's side, so whatever we last intended has to
                        // go out again. Without this the presence stays
                        // missing after a Discord restart until some later
                        // producer update — up to the tray's 5-minute poll.
                        if pending != nil { hasPending = true }
                        flush()
                    }
                }
            }
        }
    }

    // MARK: - Writing

    private func flush() {
        // Against the epoch `pending` was recorded under, not against "is it on
        // now": the READY restore and the throttle's deferred wake-up both land
        // here carrying no ticket of their own, and what they would write is
        // that payload.
        guard consentAllows(pendingEpoch) else { return }
        guard running, ready, hasPending, fd >= 0 else { return }
        // The floor limits how often NEW information is published — sampling
        // frequency is what turns a presence into a working-hours trace. Two
        // cases carry none, and making them wait points the rule backwards:
        //   - a clear removes information rather than adding it, and delaying
        //     one keeps a stale presence public for up to 15s after the user
        //     hid the clients that produced it;
        //   - a restore after a reconnect re-sends bytes Discord already had,
        //     so it discloses nothing a fresh sample would.
        // Already on the wire for this connection: not a restore, not a new
        // sample, just a repeat. Sending it would spam Discord and reset the
        // floor's clock, pushing the next real payload behind a no-op.
        if deliveredOnThisConnection == .payload(pending) {
            hasPending = false
            // Consumed here too: the reduction is already on the wire, so the
            // permission has nothing left to do, and leaving it armed would
            // hand the floor bypass to whatever ordinary sample comes next.
            consentLock.withLock { $0.floorBypass = false }
            return
        }
        let carriesNewInformation = pending != nil && pending != lastSampledPayload
        if carriesNewInformation, !consentLock.withLock({ $0.floorBypass }), let lastSent {
            let elapsed = Double(DispatchTime.now().uptimeNanoseconds - lastSent.uptimeNanoseconds)
                / 1_000_000_000
            if elapsed < publishInterval {
                // Deferred, not dropped: hiding a client has to reach the
                // published presence in the same turn, and dropping the update
                // would leave it visible until the next poll.
                let work = DispatchWorkItem { [weak self] in self?.flush() }
                throttleWork?.cancel()
                throttleWork = work
                queue.asyncAfter(
                    deadline: .now() + (publishInterval - elapsed), execute: work)
                return
            }
        }
        // Cleared only once the bytes are actually out. A write that fails
        // tears the connection down, and marking the payload sent beforehand
        // meant the newest activity was lost with the socket: the next READY
        // saw `hasPending == false` and published nothing, leaving the
        // presence stale until some later producer update. The throttle clock
        // only advances on success for the same reason — a failed send must
        // not consume the 15s window.
        if writeFrame(.frame, DiscordIPC.activityJSON(pending, pid: pid(), nonce: nonce())) {
            hasPending = false
            // Only a new sample consumes the interval. A restore or a clear
            // skips the floor on the way out precisely because it carries no
            // new information, so letting it advance the clock would throttle
            // the next genuinely changed payload from the moment of the
            // restore rather than from the last real sample — the same stale
            // presence the bypass exists to avoid, arriving by the other door.
            if carriesNewInformation { lastSent = .now() }
            // One shot, spent on the write it was granted for. Left armed it
            // would let the next ordinary sample skip the floor as well.
            consentLock.withLock { $0.floorBypass = false }
            lastSampledPayload = pending
            deliveredOnThisConnection = .payload(pending)
        }
    }

    @discardableResult
    private func writeFrame(_ op: DiscordIPC.Opcode, _ body: Data) -> Bool {
        guard fd >= 0 else { return false }
        let frame = DiscordIPC.encode(op, body)
        let socketFD = fd
        var failed = false
        frame.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let written = send(socketFD, raw.baseAddress! + offset, raw.count - offset, 0)
                if written > 0 {
                    offset += written
                    continue
                }
                if written < 0 && errno == EINTR { continue }
                writeErrno = errno
                failed = true
                return
            }
        }
        if failed { handleDisconnect() }
        return !failed
    }

    private func pid() -> Int32 { ProcessInfo.processInfo.processIdentifier }
    private func nonce() -> String { UUID().uuidString }
}
