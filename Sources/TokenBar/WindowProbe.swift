import Foundation
import TokenBarCore

/// The measurement lane behind `--window-probe`, kept deliberately.
///
/// Resolves the real quota window against real local data and reports what was
/// spent inside it, attributed, with timings. Every performance figure quoted in
/// this feature's comments — 14.93 days and 109,278 messages at 67 seconds,
/// 5.4 days and 45,844 at 4.6 — came from here, and the scan range grows with
/// recorded history, so those figures need re-taking rather than trusting.
///
/// It was headed "throwaway, not for commit" while being committed. That was
/// simply untrue, and the header is what a reader checks before believing
/// anything else in the file.
///
/// Not reachable from the shipping UI: `main.swift` runs it only for the flag
/// and exits. Its attribution folds mirror the production ones deliberately —
/// a probe that classified usage differently from the app would measure a
/// different app.
enum WindowProbe {
    /// Saturating fold over untrusted transcript counters, matching the
    /// production folds rather than diverging from them.
    ///
    /// `reduce(0, +)` traps on overflow, and the probe reads the very files
    /// `WindowMessage.tokens` is deliberately bounded for. A malformed row must
    /// not be able to kill the instrument — and worse than killing it, an
    /// unbounded `tokens - cacheRead` can wrap NEGATIVE once `tokens`
    /// saturates, which is a numerator that flows into a ratio and produces a
    /// wrong constant instead of a crash.
    private static func sum(_ values: [Int64]) -> Int64 {
        values.reduce(Int64(0)) { $0.saturatingAdding($1) }
    }

    private static func fmtDay(_ secs: Int64) -> String {
        let f = DateFormatter()
        f.dateFormat = "MM-dd HH:mm"
        return f.string(from: Date(timeIntervalSince1970: Double(secs)))
    }

    static func run() -> Never {
        do {
            let payload = try TBCore.agentUsage()

            // #359. Everything from the history file up through
            // `QuotaOverviewFold` measures clean, so what is left unmeasured is
            // the strip's own inputs. These are the exact expressions
            // `refreshWindowQuotaHalves` evaluates before it collects anything:
            // an empty `windowCardClients`, or a `visibleAgents` with no
            // windows, skips the collect and leaves the card reporting
            // "nothing recorded" about history that resolved a section below.
            print("=== #359 STRIP INPUTS ===")
            let d = UserDefaults.standard
            let tabHiddenRaw = d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""
            let limitsHiddenRaw = d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""
            let orderRaw = d.string(forKey: ClientRegistry.tabOrderKey) ?? ""
            print("  defaults domain      \(Bundle.main.bundleIdentifier ?? "(裸執行檔)")")
            print("  tabs.hidden          \(tabHiddenRaw.isEmpty ? "(空)" : tabHiddenRaw)")
            print("  limits.hidden        \(limitsHiddenRaw.isEmpty ? "(空)" : limitsHiddenRaw)")
            print("  tabs.order           \(orderRaw.isEmpty ? "(空)" : orderRaw)")
            print("  configuredClientIds  \(payload.configuredClientIds)")
            for a in payload.agents {
                print("    \(a.clientId)  source=\(a.source)"
                      + "  account=\(a.accountKey ?? "—")"
                      + "  windows=\(a.uniqueCardWindows.count)")
            }
            let tabHidden = ClientRegistry.parseIdSet(tabHiddenRaw)
            let limitsHidden = ClientRegistry.parseIdSet(limitsHiddenRaw)
            // `present` comes from the graph, which this lane does not load.
            // The production call unions it in, so this is a lower bound:
            // `windowCardClients` is at least whatever this yields.
            let cardClients = ClientRegistry.quotaClients(
                present: [], quotaIds: payload.configuredClientIds,
                tabHidden: tabHidden, orderRaw: orderRaw)
            print("  windowCardClients    \(cardClients)   ← 空的話 strip 不會被填")
            let visible = payload.agents.filter { agent in
                cardClients.contains(agent.clientId)
                    && (agent.accountKey != nil || !limitsHidden.contains(agent.clientId))
            }
            print("  visibleAgents        \(visible.map(\.clientId))")
            print("  其中有窗的           \(visible.filter { !$0.uniqueCardWindows.isEmpty }.map(\.clientId))"
                  + "   ← 空的話 collected 是空的")

            // #359. The one variable a long-lived process has and a fresh one
            // does not: publication generation drift. `refreshWindowQuotaHalves`
            // reads curves with `payload.publicationGeneration`, and the engine
            // answers a mismatch with an ERROR, not nil. That throw sets
            // `readFailed`, which skips the whole publish — so a process whose
            // generation has moved past the payload its refresh is holding
            // never publishes a strip at all, and one that launched empty stays
            // empty until relaunch. Measured rather than argued: take the
            // generation now, take it again after a poll interval, and try a
            // read with the older one.
            if CommandLine.arguments.contains("--generation-drift") {
                print("\n  --- generation drift ---")
                let gen0 = payload.publicationGeneration
                print("  第一次 publicationGeneration = \(gen0.map(String.init) ?? "nil")")
                let probeTargets = visible.flatMap { a in
                    a.uniqueCardWindows.compactMap { w in
                        w.paceStatus.windowKey.map { (a.clientId, a.accountKey, $0) }
                    }
                }
                Thread.sleep(forTimeInterval: 70)
                let again = try TBCore.agentUsage()
                let gen1 = again.publicationGeneration
                print("  70 秒後             = \(gen1.map(String.init) ?? "nil")"
                      + (gen0 == gen1 ? "   （沒有漂）" : "   ← 漂了"))
                for (client, account, key) in probeTargets.prefix(4) {
                    for (label, g) in [("舊 generation", gen0), ("新 generation", gen1)] {
                        guard let g else { continue }
                        do {
                            let c = try TBCore.quotaCurve(
                                clientId: client, accountKey: account, windowKey: key, generation: g)
                            print("    \(client)/\(key) 用\(label)：\(c == nil ? "nil（無歷史）" : "\(c!.points.count) 點")")
                        } catch {
                            print("    \(client)/\(key) 用\(label)：**丟錯** \(error)"
                                  + "   ← 這個窗保留上次的值，其他窗照常發佈")
                        }
                    }
                }
            }
            print("")

            let now = Int64(Date().timeIntervalSince1970 * 1000)
            let fmt = ISO8601DateFormatter()
            fmt.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            let plain = ISO8601DateFormatter()

            print("=== HOURLY COST（同一個 process 內連續呼叫）===")
            for i in 1...3 {
                let t = Date()
                let r = try TBCore.hourlyReport(year: nil, clients: nil)
                print(String(format: "  第 %d 次  %8.0f ms  %d 筆", i,
                             Date().timeIntervalSince(t) * 1000, r.entries.count))
            }
            var t2 = Date()
            _ = try TBCore.hourlyReport(year: nil, clients: ["claude"])
            print(String(format: "  換 client slice（claude）  %8.0f ms",
                         Date().timeIntervalSince(t2) * 1000))
            t2 = Date()
            _ = try TBCore.hourlyReport(year: "2026", clients: nil)
            print(String(format: "  換 year slice（2026）      %8.0f ms",
                         Date().timeIntervalSince(t2) * 1000))
            t2 = Date()
            _ = try TBCore.graph(year: nil)
            print(String(format: "  對照：graph(nil)           %8.0f ms",
                         Date().timeIntervalSince(t2) * 1000))

            print("\n=== HEATMAP（星期 x 時段，來自 hourly report 的完整歷史）===")
            let hStart = Date()
            let hourly = try TBCore.hourlyReport(year: nil, clients: nil)
            print(String(format: "  hourlyReport 取得 %.0f ms，%d 個時段",
                         Date().timeIntervalSince(hStart) * 1000, hourly.entries.count))

            var grid = Array(repeating: Array(repeating: 0.0, count: 24), count: 7)
            var days = Set<String>()
            let df = DateFormatter()
            df.dateFormat = "yyyy-MM-dd HH:mm"
            var cal = Calendar(identifier: .gregorian)
            cal.firstWeekday = 2
            for e in hourly.entries {
                guard let d = df.date(from: e.hour) else { continue }
                let wd = (cal.component(.weekday, from: d) + 5) % 7   // 0 = 週一
                let hr = cal.component(.hour, from: d)
                grid[wd][hr] += e.cost
                days.insert(String(e.hour.prefix(10)))
            }
            let flat = grid.flatMap { $0 }
            let peak = flat.max() ?? 0
            let filled = flat.filter { $0 > 0 }.count
            print(String(format: "  涵蓋 %d 天、%d/168 格有資料（%.0f%%）、單格峰值 $%.2f",
                         days.count, filled, Double(filled) / 168 * 100, peak))
            let ramp = [" ", "·", "░", "▒", "▓", "█"]
            print("        00  02  04  06  08  10  12  14  16  18  20  22")
            let names = ["週一","週二","週三","週四","週五","週六","週日"]
            for (i, row) in grid.enumerated() {
                var line = "   \(names[i]) "
                for h in stride(from: 0, to: 24, by: 1) {
                    let v = peak > 0 ? row[h] / peak : 0
                    let idx = v == 0 ? 0 : min(5, Int(v * 5) + 1)
                    line += ramp[idx] + ramp[idx]
                }
                print(line)
            }

            print("\n=== CYCLE HISTORY（每個窗的歷史裡有幾個重置週期）===")
            for agent in payload.agents {
                for w in agent.uniqueCardWindows {
                    guard let key = w.paceStatus.windowKey,
                          let gen = payload.publicationGeneration,
                          let curve = (try? TBCore.quotaCurve(
                              clientId: agent.clientId, accountKey: agent.accountKey, windowKey: key, generation: gen)) ?? nil,
                          !curve.points.isEmpty
                    else { continue }
                    var byCycle: [Int64: [QuotaCurvePoint]] = [:]
                    for pt in curve.points { byCycle[pt.resetAt, default: []].append(pt) }
                    let cycles = byCycle.keys.sorted()
                    let span = (curve.points.map(\.sampledAt).min() ?? 0,
                                curve.points.map(\.sampledAt).max() ?? 0)
                    print(String(format: "\n  %@ / %@  共 %d 點、%d 個週期、橫跨 %.1f 天",
                                 agent.clientId as NSString, w.cardId as NSString,
                                 curve.points.count, cycles.count,
                                 Double(span.1 - span.0) / 86400))
                    for r in cycles.suffix(6) {
                        let pts = byCycle[r]!.sorted { $0.sampledAt < $1.sampledAt }
                        let peak = pts.map(\.usedPercent).max() ?? 0
                        let dur = pts[0].durationSeconds
                        print(String(format:
                            "      重置 %@  取樣 %3d  峰值用量 %5.1f%%  窗長 %.0f 分  觀測涵蓋 %.0f%%",
                            fmtDay(r) as NSString, pts.count, peak, Double(dur) / 60,
                            Double(pts.last!.sampledAt - pts[0].sampledAt) / Double(max(dur,1)) * 100))
                    }
                }
            }

            print("\n=== SCAN BOUND（#229：上限前後，掃描要往回走多久）===")
            print("  每個窗的訊息掃描起點是最舊的 considered cycle。上限之前是引擎保留的"
                  + "全部 128 個，之後是最新的 \(QuotaHistoryFold.consideredCycles) 個。")
            for agent in payload.agents {
                for w in agent.uniqueCardWindows {
                    guard let key = w.paceStatus.windowKey,
                          let gen = payload.publicationGeneration,
                          let curve = (try? TBCore.quotaCurve(
                              clientId: agent.clientId, accountKey: agent.accountKey, windowKey: key, generation: gen)) ?? nil,
                          !curve.points.isEmpty
                    else { continue }
                    let full = QuotaHistoryFold.cycles(
                        points: curve.points)
                    let capped = QuotaHistoryFold.considered(full)
                    guard let oldestUncapped = full.last?.evidenceStartMs,
                          let oldestCapped = capped.last?.evidenceStartMs
                    else { continue }
                    let nowS = Int64(Date().timeIntervalSince1970)
                    let beforeDays = Double(nowS - oldestUncapped / 1000) / 86400
                    let afterDays = Double(nowS - oldestCapped / 1000) / 86400
                    print(String(format:
                        "  %@ / %@  週期 %d → %d，掃描回溯 %.2f 天 → %.2f 天%@",
                        agent.clientId as NSString, w.cardId as NSString,
                        full.count, capped.count, beforeDays, afterDays,
                        (full.count > capped.count ? "  ← 收窄" : "") as NSString))
                    // Windows with enough history to sweep. Deliberately not
                    // "windows the cap currently bites": the constant is set
                    // from where the estimate collapses, which has to stay
                    // measurable while the cap sits above it.
                    guard full.count >= 8 else { continue }
                    // `evidenceStartMs` is already milliseconds; `nowS` is not.
                    // This agent's own account, not every account's. The
                    // sweep tunes `consideredCycles` against the shipping
                    // estimate, and the shipping estimate divides one
                    // account's usage by that account's quota movement; a
                    // wider scan here would tune the constant against a
                    // numerator the app never uses.
                    guard let messages = try? TBCore.windowUsage(
                        accountKey: agent.accountKey,
                        from: oldestUncapped, until: nowS * 1000).messages
                    else { continue }
                    let confirmed = UsageAttribution.confirmed().records
                    func estimate(_ set: [QuotaCycle]) -> WindowEquivalence.Row {
                        let admitted = set.filter {
                            WindowEquivalence.deltaQualifies($0.usedPercent, runs: $0.risingRuns)
                                && $0.observedFraction >= WindowEquivalence.minimumObservedFraction
                        }
                        let spans = QuotaHistoryFold.spans(
                            cycles: admitted, messages: messages,
                            subscription: agent.clientId,
                            // The probe measures the SHIPPING calculation, so
                            // it has to narrow the same way. A sweep run on a
                            // scoped window without this tunes a constant
                            // against numbers the app never computes.
                            modelScope: w.modelScope, confirmed: confirmed)
                        return WindowEquivalence.aggregate(
                            // Same subscription the spans above were narrowed
                            // to; the probe measures the shipping calculation,
                            // so it takes the shipping overload.
                            subscription: agent.clientId, records: confirmed,
                            cycles: zip(admitted, spans).map { cycle, span in
                                WindowEquivalence.Cycle(
                                    deltaPercent: cycle.usedPercent,
                                    spanTokens: span.tokens, spanCost: span.cost,
                                    observedFraction: cycle.observedFraction,
                                    risingRuns: cycle.risingRuns)
                            })
                    }
                    // The sweep that set `consideredCycles`. Re-run it before
                    // moving that constant: the estimate does not degrade
                    // gracefully as the pool shrinks, it stops existing.
                    for cap in [8, 12, 16, 20, 24, 28, 32, full.count] {
                        print("      上限 \(cap >= full.count ? "無" : String(cap))：\(estimate(Array(full.prefix(cap))))")
                    }
                }
            }

            print("\n=== PERF（我加進 main actor 的兩件事）===")
            var perfT = Date()
            var curveMs: [(String, Double)] = []
            for agent in payload.agents {
                for w in agent.uniqueCardWindows {
                    let t = Date()
                    _ = WindowCardLoader.curveSamples(
                        payload: payload, clientId: agent.clientId, accountKey: agent.accountKey, window: w,
                        curve: { c, a, k, g in
                            (try? TBCore.quotaCurve(clientId: c, accountKey: a, windowKey: k, generation: g)) ?? nil
                        }, nowMs: now) ?? []
                    curveMs.append(("\(agent.clientId)|\(w.cardId)",
                                    Date().timeIntervalSince(t) * 1000))
                }
            }
            for (k, ms) in curveMs.sorted(by: { $0.1 > $1.1 }) {
                print(String(format: "  curveSamples  %8.1f ms  %@", ms, k as NSString))
            }
            print(String(format: "  === 整圈合計 %.1f ms（每次 poll 都同步跑一次）",
                         Date().timeIntervalSince(perfT) * 1000))

            perfT = Date()
            let sw = payload.agents[0].uniqueCardWindows[0]
            for _ in 0..<1000 {
                _ = AgentLimitsCard.sparklineInterval(window: sw, samples: [], nowMs: now)
            }
            print(String(format: "  sparklineInterval  %.4f ms/次（body 每次重算每列一次）",
                         Date().timeIntervalSince(perfT)))

            print("\n=== SPARK ELIGIBILITY（Agent limits 每列會畫線還是長條）===")
            for agent in payload.agents {
                for w in agent.uniqueCardWindows {
                    let samples = WindowCardLoader.curveSamples(
                        payload: payload, clientId: agent.clientId, accountKey: agent.accountKey, window: w,
                        curve: { c, a, k, g in
                            (try? TBCore.quotaCurve(clientId: c, accountKey: a, windowKey: k, generation: g)) ?? nil
                        }, nowMs: now) ?? []
                    let res = WindowCardLoader.resolution(
                        window: w, nowMs: now, firstUsageAfterReset: nil)
                    let iv = WindowCardLoader.interval(res)
                    let inside = iv.map { i in
                        samples.filter { $0.atMs >= i.start && $0.atMs <= now }.count } ?? 0
                    let verdict = AgentLimitsCard.sparklineInterval(
                        window: w, samples: samples, nowMs: now) != nil ? "LINE" : "BAR "
                    print(String(format: "  %@  %-10@ %-24@ samples=%3d  interval=%@  inside=%3d",
                                 verdict as NSString, agent.clientId as NSString,
                                 w.cardId as NSString, samples.count,
                                 (iv == nil ? "nil   " : "yes   ") as NSString, inside))
                }
            }

            print("\n=== SPARK SHAPE（線實際佔多寬、y 跨多少）===")
            for agent in payload.agents {
                for w in agent.uniqueCardWindows {
                    let samples = WindowCardLoader.curveSamples(
                        payload: payload, clientId: agent.clientId, accountKey: agent.accountKey, window: w,
                        curve: { c, a, k, g in
                            (try? TBCore.quotaCurve(clientId: c, accountKey: a, windowKey: k, generation: g)) ?? nil
                        }, nowMs: now) ?? []
                    guard let iv = AgentLimitsCard.sparklineInterval(
                        window: w, samples: samples, nowMs: now) else { continue }
                    let g = WindowCardGeometry.quotaGeometry(
                        windowStartMs: iv.start, windowEndMs: iv.end,
                        nowMs: min(now, iv.end), samples: samples, metric: .remaining)
                    let ys = g.curve.map(\.y)
                    let pace = UsagePace.compute(window: w, mode: .historical)
                    print(String(format:
                        "  %-10@ %-20@ x %.2f→%.2f (%.0f%% 寬)  y %.1f→%.1f (跨 %.1f 點, 26px 裡 %.1fpx)  pace 期望剩 %@",
                        agent.clientId as NSString, w.cardId as NSString,
                        g.firstSampleX, g.nowX, (g.nowX - g.firstSampleX) * 100,
                        ys.max() ?? 0, ys.min() ?? 0, (ys.max() ?? 0) - (ys.min() ?? 0),
                        ((ys.max() ?? 0) - (ys.min() ?? 0)) / 100 * 22,
                        (pace.map { String(format: "%.1f", 100 - $0.expectedUsedPercent) } ?? "—") as NSString))
                }
            }

            print("\n=== quota 窗解析（真實資料）===\n")
            print("  agents in payload: \(payload.agents.map(\.clientId))")
            for a in payload.agents where a.error != nil || a.windows.isEmpty {
                print("    ⚠️ \(a.clientId): error=\(a.error ?? "nil") windows=\(a.windows.count)")
            }
            // The account travels with the window. It used to be dropped here,
            // and the SCAN COST section below carried a comment admitting it
            // read the primary account only — an admission is not a fix when
            // the payload lists an extra account's windows two lines up.
            var resolved: [(String, String?, String, WindowResolution)] = []
            var windowKeys: [String: String?] = [:]

            for agent in payload.agents {
                for w in agent.windows {
                    let resetMs = w.resetsAt.flatMap { s -> Int64? in
                        let d = fmt.date(from: s) ?? plain.date(from: s)
                        return d.map { Int64($0.timeIntervalSince1970 * 1000) }
                    }
                    let durMs = w.durationSeconds.map { $0 * 1000 }

                    // First pass: no usage probe. Only ACTIVE / UNAVAILABLE are
                    // decidable without it, which is exactly the point — the
                    // probe is bounded by R1 and must not run when R is stale.
                    let pre = WindowResolver.resolve(
                        resetsAtMs: resetMs, durationMs: durMs, now: now,
                        firstUsageAfterReset: nil)

                    // Only when `pre` is .idle does R1 hold and a usage probe
                    // become legal — .idle means R is past but within one D.
                    var final = pre
                    if case .idle = pre, let r = resetMs, let d = durMs {
                        let usage = try TBCore.windowUsage(
                            accountKey: agent.accountKey, from: r, until: now)
                        let t0 = WindowResolver.firstUsageAfterReset(
                            messages: usage.messages, resetMs: r)
                        final = WindowResolver.resolve(
                            resetsAtMs: r, durationMs: d, now: now,
                            firstUsageAfterReset: t0)
                    }
                    resolved.append((agent.clientId, agent.accountKey, w.cardId, final))
                    windowKeys["\(agent.clientId)|\(w.cardId)"] = w.paceStatus.windowKey
                }
            }

            print("  resetsAt 實測：")
            for a in payload.agents { for w in a.windows where a.clientId == "claude" {
                print("    \(w.cardId): resetsAt=\(w.resetsAt ?? "nil") "
                      + "dur=\(w.durationSeconds.map(String.init) ?? "nil") "
                      + "used=\(w.usedPercent) remain=\(w.remainingPercent) "
                      + "pace=\(w.paceStatus.state) reason=\(String(describing: w.paceStatus.reason))")
            } }
            print("  label 實測值：")
            for a in payload.agents { for w in a.windows {
                print("    \(a.clientId)|\(w.cardId)  label=\(String(reflecting: w.label))")
            } }
            print("  windowKey 實測值：")
            for (k, v) in windowKeys.sorted(by: { $0.key < $1.key }) {
                print("    \(k)  →  \(v ?? "nil")")
            }
            for (client, accountKey, card, state) in resolved {
                print(String(format: "  %-16@ %-10@ %-22@ %@",
                             client as NSString,
                             (accountKey.map { ($0 as NSString).lastPathComponent }
                                ?? "primary") as NSString,
                             card as NSString,
                             describe(state, now: now) as NSString))
            }

            // Now the number, for every window that resolved to a real interval.
            let confirmed = UsageAttribution.confirmed().records
            print("=== ADMITTED（出貨的 curveSamples 實際收到幾點）===")
            for agent in payload.agents { for w in agent.windows {
                guard let k = w.paceStatus.windowKey,
                      let rMs = w.resetsAt.flatMap({ t -> Int64? in
                          let f = ISO8601DateFormatter()
                          f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
                          return (f.date(from: t) ?? ISO8601DateFormatter().date(from: t))
                              .map { Int64($0.timeIntervalSince1970 * 1000) }
                      }),
                      let dS = w.durationSeconds,
                      let c = (try? TBCore.quotaCurve(
                          clientId: agent.clientId, accountKey: agent.accountKey, windowKey: k,
                          generation: payload.publicationGeneration ?? 0)) ?? nil
                else { continue }
                let lo = (rMs - dS * 1000) / 1000, hi = now / 1000
                let admitted = c.points.filter { $0.sampledAt >= lo && $0.sampledAt <= hi }
                let timeOnly = c.points.filter { $0.sampledAt >= lo && $0.sampledAt <= hi }
                print(String(format: "  %-10@ %-20@ 收到 %3d 點（僅時間範圍 %3d）%@",
                             agent.clientId as NSString, w.cardId as NSString,
                             admitted.count, timeOnly.count,
                             (admitted.isEmpty && !timeOnly.isEmpty
                                ? "  ⚠️ 守衛把全部濾光了" : "") as NSString))
            } }

            print("\n=== CYCLE MATCH（我的守衛比對的兩個值）===")
            for agent in payload.agents { for w in agent.windows {
                guard let k = w.paceStatus.windowKey,
                      let rMs = w.resetsAt.flatMap({ t -> Int64? in
                          let f = ISO8601DateFormatter()
                          f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
                          return (f.date(from: t) ?? ISO8601DateFormatter().date(from: t))
                              .map { Int64($0.timeIntervalSince1970 * 1000) }
                      }),
                      let c = (try? TBCore.quotaCurve(
                          clientId: agent.clientId, accountKey: agent.accountKey, windowKey: k,
                          generation: payload.publicationGeneration ?? 0)) ?? nil,
                      let last = c.points.last
                else { continue }
                print(String(format: "  %-10@ %-20@ resetsAt→%ld  curve.resetAt=%ld  差 %ld 秒  相符=%@",
                             agent.clientId as NSString, w.cardId as NSString,
                             rMs / 1000, last.resetAt, (rMs / 1000) - last.resetAt,
                             ((rMs / 1000) == last.resetAt ? "yes" : "NO") as NSString))
            } }

            print("\n=== MIXED CYCLE（卡片實際會取到的樣本裡混了幾個週期）===")
            for agent in payload.agents { for w in agent.windows {
                guard let k = w.paceStatus.windowKey,
                      let r = w.resetsAt.flatMap({ t -> Int64? in
                          let f = ISO8601DateFormatter()
                          f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
                          return (f.date(from: t) ?? ISO8601DateFormatter().date(from: t))
                              .map { Int64($0.timeIntervalSince1970) }
                      }),
                      let d = w.durationSeconds,
                      let c = (try? TBCore.quotaCurve(
                          clientId: agent.clientId, accountKey: agent.accountKey, windowKey: k,
                          generation: payload.publicationGeneration ?? 0)) ?? nil
                else { continue }
                // Exactly what curveSamples does today: time range only.
                let inRange = c.points.filter {
                    $0.sampledAt >= r - d && $0.sampledAt <= now / 1000
                }
                let cycles = Set(inRange.map(\.resetAt))
                print(String(format: "  %-10@ %-20@ 取到 %3d 點，來自 %d 個週期%@",
                             agent.clientId as NSString, w.cardId as NSString,
                             inRange.count, cycles.count,
                             (cycles.count > 1 ? "  ⚠️ 會畫出假的補額" : "") as NSString))
            } }

            print("\n=== GAP DIST（整條序列的取樣間隔，分鐘）===")
            for agent in payload.agents { for w in agent.windows {
                guard let k = w.paceStatus.windowKey,
                      let c = (try? TBCore.quotaCurve(
                          clientId: agent.clientId, accountKey: agent.accountKey, windowKey: k,
                          generation: payload.publicationGeneration ?? 0)) ?? nil,
                      c.points.count > 3 else { continue }
                let pts = c.points.sorted { $0.sampledAt < $1.sampledAt }
                var gaps: [Int64] = [], crossReset = 0
                for (a, b) in zip(pts, pts.dropFirst()) {
                    gaps.append((b.sampledAt - a.sampledAt) / 60)
                    if a.resetAt != b.resetAt { crossReset += 1 }
                }
                // Same-cycle only: a cross-reset gap is a different thing.
                var inCycle: [Int64] = []
                for (a, b) in zip(pts, pts.dropFirst()) where a.resetAt == b.resetAt {
                    inCycle.append((b.sampledAt - a.sampledAt) / 60)
                }
                let ic = inCycle.sorted()
                let icMed = ic.isEmpty ? 0 : ic[ic.count / 2]
                print(String(format: "     └ 同週期內：n=%3d 中位 %4ld 分 最大 %5ld 分 (%.0fx)",
                             ic.count, icMed, ic.last ?? 0,
                             Double(ic.last ?? 0) / Double(max(icMed, 1))))
                let sorted = gaps.sorted()
                let med = sorted[sorted.count / 2]
                let mx = sorted.last ?? 0
                print(String(format: "  %-10@ %-20@ n=%3d  中位 %4ld 分  最大 %6ld 分 (%.0fx)  跨 reset %d 段",
                             agent.clientId as NSString, w.cardId as NSString, pts.count,
                             med, mx, Double(mx) / Double(max(med, 1)), crossReset))
            } }

            print("\n=== CURVE COST（額度線，讀持久化的取樣）===")
            for agent in payload.agents { for w in agent.windows {
                guard let k = w.paceStatus.windowKey else { continue }
                let t = Date()
                let c = (try? TBCore.quotaCurve(clientId: agent.clientId, accountKey: agent.accountKey, windowKey: k,
                                                generation: payload.publicationGeneration ?? 0)) ?? nil
                let ms = Date().timeIntervalSince(t) * 1000
                print(String(format: "  %-10@ %-22@ %5.1f ms  %d 點  resetAt/dur 齊全=%@",
                             agent.clientId as NSString, w.cardId as NSString, ms,
                             c?.points.count ?? 0,
                             ((c?.points.first.map { $0.resetAt > 0 && $0.durationSeconds > 0 }) ?? false)
                                ? "yes" : "no"))
            } }

            // One scan over the widest window, then filter per card in memory.
            print("=== UNION SCAN（掃一次最寬範圍，各卡自行過濾）===")
            var widest = now
            for (_, _, _, st) in resolved { if let (a, _) = interval(st) { widest = min(widest, a) } }
            // One scan PER ACCOUNT over the widest window, then filter per
            // card in memory. Not one scan for everybody: a window belongs to
            // an account, and the app no longer has a scan that spans them.
            var t0 = Date()
            var unionMs = 0.0
            var byAccount: [String: WindowUsage] = [:]
            for account in Set(resolved.map { $0.1 ?? "" }) {
                t0 = Date()
                byAccount[account] = try TBCore.windowUsage(
                    accountKey: account.isEmpty ? nil : account, from: widest, until: now)
                unionMs += Date().timeIntervalSince(t0) * 1000
            }
            let unionCount = byAccount.values.reduce(0) { $0 + $1.messages.count }
            print(String(format: "  掃描 %d 個帳號 × %.0f 分鐘範圍：%ld 筆，%.0f ms",
                         byAccount.count, Double(now - widest) / 60000, unionCount, unionMs))
            var filterTotal = 0.0
            for (cl, ak, cd, st) in resolved {
                guard let (a, b) = interval(st),
                      let all = byAccount[ak ?? ""] else { continue }
                t0 = Date()
                let sub = all.messages.filter { $0.timestamp > a && $0.timestamp <= min(b, now) }
                let ms = Date().timeIntervalSince(t0) * 1000
                filterTotal += ms
                print(String(format: "    %-10@ %-22@ 過濾出 %6ld 筆  %.1f ms",
                             cl as NSString, cd as NSString, sub.count, ms))
            }
            print(String(format: "  合計：%.0f ms（vs 逐窗各掃一次）", unionMs + filterTotal))

            print("\n=== SCAN COST（每次載入一次，非每次 hover）===")
            for (client, accountKey, card, state) in resolved {
                guard let (start, end) = interval(state) else { continue }
                var t0 = Date()
                let usage = try TBCore.windowUsage(
                    accountKey: accountKey, from: start, until: min(end, now))
                let scanMs = Date().timeIntervalSince(t0) * 1000
                let conf = UsageAttribution.confirmed().records
                t0 = Date()
                let mineHere = usage.messages.filter {
                    UsageAttribution.resolve(client: $0.client, provider: $0.providerId,
                                             model: $0.modelId, records: conf) == .assigned(client)
                }
                let foldMs = Date().timeIntervalSince(t0) * 1000
                t0 = Date()
                let g = WindowCardGeometry.usageGeometry(
                    windowStartMs: start, windowEndMs: end, nowMs: min(end, now),
                    samples: [], messages: mineHere)
                let geoMs = Date().timeIntervalSince(t0) * 1000
                print(String(format: "  %-10@ %-24@ %6ld 筆  掃描 %7.1f ms  歸屬 %6.1f ms  幾何 %5.1f ms  → 本訂閱 %ld 筆",
                             client as NSString, card as NSString, usage.messages.count,
                             scanMs, foldMs, geoMs, mineHere.count))
                _ = g
                let (assigned, excluded, unassigned) = usage.totals(confirmed: confirmed)
                print("\n--- \(client) / \(card)  \(usage.messages.count) 筆"
                      + (usage.undatedCount == 0 ? ""
                         : "（另有 \(usage.undatedCount) 筆無時戳、未計入）"))
                for t in assigned {
                    print(String(format: "    %-12@ %14ld token   $%.2f",
                                 t.target as NSString, t.tokens, t.cost))
                }
                for (label, t) in [("excluded", excluded), ("unassigned", unassigned)]
                where t.tokens != 0 {
                    print(String(format: "    %-12@ %14ld token   $%.2f",
                                 label as NSString, t.tokens, t.cost))
                }
                let n = 12
                let width = max((min(end, now) - start) / Int64(n), 1)
                let curve = usage.buckets(from: start, bucketMs: width, count: n)
                print("    曲線（\(width / 60000) 分一桶，百萬 token）: "
                      + curve.map { String($0 / 1_000_000) }.joined(separator: " "))

                // Cost of the two halves, on real data. The old body recomputed
                // BOTH on every hover event.
                if card.hasPrefix("session") {
                    let confirmed3 = UsageAttribution.confirmed().records
                    var t0 = Date()
                    let mineNow = usage.messages.filter {
                        UsageAttribution.resolve(
                            client: $0.client, provider: $0.providerId, model: $0.modelId,
                            records: confirmed3) == .assigned(client)
                    }
                    let attrMs = Date().timeIntervalSince(t0) * 1000
                    let qs = (try? TBCore.quotaCurve(
                        clientId: client, accountKey: nil, windowKey: card,
                        generation: payload.publicationGeneration ?? 0))??.points
                        .filter { $0.sampledAt >= start / 1000 && $0.sampledAt <= now / 1000 }
                        .map { QuotaSample(atMs: $0.sampledAt * 1000, usedPercent: $0.usedPercent) }
                        ?? []
                    t0 = Date()
                    let ug = WindowCardGeometry.usageGeometry(
                        windowStartMs: start, windowEndMs: end, nowMs: min(end, now),
                        samples: qs, messages: mineNow)
                    let usageMs = Date().timeIntervalSince(t0) * 1000
                    t0 = Date()
                    for _ in 0..<100 {
                        _ = WindowCardGeometry.quotaGeometry(
                            windowStartMs: start, windowEndMs: end, nowMs: min(end, now),
                            samples: qs, metric: .remaining)
                    }
                    let quotaMs = Date().timeIntervalSince(t0) * 1000 / 100
                    print(String(format:
                        "    ⏱ 每次 body 的成本：歸屬 %.1f ms ＋ bars/zones %.1f ms（舊）"
                        + "  vs  曲線 %.3f ms（新）｜%d 筆訊息、%d 個 zone",
                        attrMs, usageMs, quotaMs, mineNow.count, ug.hits.count))
                }

                // Does the quota line have real samples inside this window?
                guard let key = windowKeys["\(client)|\(card)"] ?? nil else {
                    print("    額度線：此窗無 windowKey")
                    continue
                }
                let qc = try TBCore.quotaCurve(
                    clientId: client, accountKey: nil, windowKey: key, generation: payload.publicationGeneration ?? 0)
                guard let qc else { print("    額度線：series 未綁定"); continue }
                // sampledAt/resetAt are SECONDS — the decoder subtracts
                // durationSeconds from resetAt directly (QuotaCurve.swift:64).
                let inWindow = qc.points.filter {
                    $0.sampledAt >= start / 1000 && $0.sampledAt <= now / 1000
                }
                print("    額度線：窗內 \(inWindow.count) 點 / 全序列 \(qc.coverage.sampleCount) 點")
                if !inWindow.isEmpty {
                    print("      " + inWindow.map { String(format: "%.3f%%", $0.usedPercent) }
                        .joined(separator: " → "))
                    // The pairing check: between consecutive quota samples, how
                    // many tokens assigned to THIS subscription were spent? If
                    // the two disagree, the gap is usage this machine can't see.
                    // Equivalence ratio, measured over the sampled span only —
                    // the percent delta and the tokens must cover the same
                    // interval or the ratio is meaningless.
                    let lo = inWindow.first!, hi2 = inWindow.last!
                    let dPct = hi2.usedPercent - lo.usedPercent
                    let spanMine = usage.messages
                        .filter { $0.timestamp > lo.sampledAt * 1000
                                  && $0.timestamp <= hi2.sampledAt * 1000 }
                        .filter { m in
                            UsageAttribution.resolve(
                                client: m.client, provider: m.providerId,
                                model: m.modelId, records: confirmed) == .assigned(client)
                        }
                    // Everything except cache read: measured to separate moved
                    // from still buckets 4.5x, vs 1.2x for input+output+reasoning.
                    let spanFresh = sum(spanMine.map(\.tokensExCacheRead))
                    let spanCost = spanMine.map(\.cost).reduce(0, +)
                    if dPct <= 0 {
                        print("      等值比：額度未移動（Δ \(dPct)%），無法計算")
                    } else if spanFresh == 0 {
                        print("      等值比：額度動了 \(Int(dPct))% 但本機 0 token，配對破裂，不得顯示")
                    } else {
                        print(String(format:
                            "      等值比（Δ%.0f%% 上量得）: 1%% ≈ %ld 新鮮 token / $%.2f  |  10%% ≈ %ld / $%.2f  |  整窗滿 ≈ $%.0f",
                            dPct, Int64(Double(spanFresh) / dPct), spanCost / dPct,
                            Int64(Double(spanFresh) / dPct * 10), spanCost / dPct * 10,
                            spanCost / dPct * 100))
                    }
                    // Which token classes actually track the quota? cacheWrite
                    // is the trap: it is a cache field by name but costs 1.25x
                    // base input, so grouping it with cacheRead throws away an
                    // expensive class. Fit all three against the same delta.
                    guard dPct > 0 else {
                        print("      （Δ=0，跳過等值比與分辨力）")
                        continue
                    }
                    var fA: Int64 = 0, fB: Int64 = 0, fC: Int64 = 0
                    for (a, b) in zip(inWindow, inWindow.dropFirst()) {
                        let l = a.sampledAt * 1000, h = b.sampledAt * 1000
                        for m in usage.messages
                            where m.timestamp > l && m.timestamp <= h
                            && UsageAttribution.resolve(
                                client: m.client, provider: m.providerId,
                                model: m.modelId, records: confirmed) == .assigned(client) {
                            fA = sum([fA, m.input, m.output, m.reasoning])
                            fB = sum([fB, m.input, m.output, m.reasoning, m.cacheWrite])
                            fC = fC.saturatingAdding(m.tokens)
                        }
                    }
                    print(String(format:
                        "      每 1%% 等值：in+out+reas %10ld  | ＋cacheWrite %10ld  | 全部 %12ld",
                        Int64(Double(fA) / dPct), Int64(Double(fB) / dPct),
                        Int64(Double(fC) / dPct)))
                    print(String(format:
                        "      cacheWrite 佔比 %.1f%% of 新鮮 | cacheRead 佔比 %.0fx 新鮮",
                        Double(fB - fA) / Double(max(fA, 1)) * 100,
                        Double(fC - fB) / Double(max(fA, 1))))
                    // Decisive test: which quantity separates the buckets where
                    // quota moved from the ones where it did not? A hand-picked
                    // token subset guesses the weights; `cost` already carries
                    // them from the pricing table.
                    struct Cand { let name: String; var moved: [Double] = []; var still: [Double] = [] }
                    var cands = [Cand(name: "in+out+reas"), Cand(name: "＋cacheWrite"),
                                 Cand(name: "全部 token"), Cand(name: "成本 $")]
                    for (a, b) in zip(inWindow, inWindow.dropFirst()) {
                        let l = a.sampledAt * 1000, h = b.sampledAt * 1000
                        var v = [0.0, 0.0, 0.0, 0.0]
                        for m in usage.messages
                            where m.timestamp > l && m.timestamp <= h
                            && UsageAttribution.resolve(
                                client: m.client, provider: m.providerId,
                                model: m.modelId, records: confirmed) == .assigned(client) {
                            v[0] += Double(sum([m.input, m.output, m.reasoning]))
                            v[1] += Double(sum([m.input, m.output, m.reasoning, m.cacheWrite]))
                            v[2] += Double(m.tokens)
                            v[3] += m.cost
                        }
                        let moved = b.usedPercent > a.usedPercent
                        for i in 0..<4 { if moved { cands[i].moved.append(v[i]) }
                                         else { cands[i].still.append(v[i]) } }
                    }
                    print("      分辨力（額度有動 vs 沒動的 bucket 中位數）:")
                    for c2 in cands {
                        func med(_ a: [Double]) -> Double {
                            guard !a.isEmpty else { return 0 }
                            let s = a.sorted(); return s[s.count / 2]
                        }
                        let mv = med(c2.moved), st = med(c2.still)
                        let ratio = st > 0 ? mv / st : Double.infinity
                        print(String(format: "        %-14@ 動 %12.2f  靜 %12.2f  倍率 %@",
                                     c2.name as NSString, mv, st,
                                     (ratio.isFinite ? String(format: "%.1fx", ratio)
                                      : "∞（靜止組全為 0）") as NSString))
                    }
                    // Paste-ready for the design mock: gaps / fresh / cost per
                    // interval, so no number in the mock is ever invented.
                    var gaps: [Int] = [], freshes: [Int64] = [], costs: [Double] = []
                    var kinds: [[Int64]] = []
                    for (a, b) in zip(inWindow, inWindow.dropFirst()) {
                        let l = a.sampledAt * 1000, h = b.sampledAt * 1000
                        let ms = usage.messages
                            .filter { $0.timestamp > l && $0.timestamp <= h }
                            .filter { m in
                                UsageAttribution.resolve(
                                    client: m.client, provider: m.providerId,
                                    model: m.modelId, records: confirmed) == .assigned(client)
                            }
                        gaps.append(Int((h - l) / 60000))
                        freshes.append(sum(ms.map(\.tokensExCacheRead)))
                        costs.append(ms.map(\.cost).reduce(0, +))
                        kinds.append([sum(ms.map(\.input)), sum(ms.map(\.output)),
                                      sum(ms.map(\.cacheRead)), sum(ms.map(\.cacheWrite)),
                                      sum(ms.map(\.reasoning))])
                    }
                    print("      JSON: {\"used\":\(inWindow.map { Int($0.usedPercent) }), "
                          + "\"gaps\":\(gaps), \"fresh\":\(freshes), "
                          + "\"cost\":\(costs.map { (($0 * 100).rounded()) / 100 }), "
                          + "\"kinds\":\(kinds), "
                          + "\"windowMin\":\((end - start) / 60000), "
                          + "\"elapsedMin\":\((now - start) / 60000)}")
                    print("      配對（相鄰取樣之間：額度 Δ% vs 本訂閱 token）:")
                    for (a, b) in zip(inWindow, inWindow.dropFirst()) {
                        let lo = a.sampledAt * 1000, hi = b.sampledAt * 1000
                        let mine = usage.messages
                            .filter { $0.timestamp > lo && $0.timestamp <= hi }
                            .filter { m in
                                UsageAttribution.resolve(
                                    client: m.client, provider: m.providerId,
                                    model: m.modelId, records: confirmed) == .assigned(client)
                            }
                        // Split it: cache reads dwarf everything by volume but
                        // cost a fraction of the quota, so a bar sized by total
                        // tokens cannot track the line by construction.
                        let spent = sum(mine.map(\.tokens))
                        let fresh = sum(mine.map { sum([$0.input, $0.output, $0.reasoning]) })
                        let cacheR = sum(mine.map(\.cacheRead))
                        print(String(format: "        +%.3f%%  總 %9ld (新鮮 %8ld, cacheR %9ld)  (%.0f 分)",
                                     b.usedPercent - a.usedPercent, spent, fresh, cacheR,
                                     Double(hi - lo) / 60000))
                    }
                }
            }
            exit(0)
        } catch {
            FileHandle.standardError.write(Data("window probe failed: \(error)\n".utf8))
            exit(1)
        }
    }

    private static func interval(_ s: WindowResolution) -> (Int64, Int64)? {
        switch s {
        case let .active(start, end), let .inferred(start, end): return (start, end)
        case .idle, .unavailable: return nil
        }
    }

    private static func describe(_ s: WindowResolution, now: Int64) -> String {
        switch s {
        case let .active(start, end):
            return "ACTIVE       剩 \((end - now) / 60000) 分  窗長 \((end - start) / 60000) 分"
        case .idle: return "IDLE         窗已結束、其後無用量"
        case let .inferred(start, end):
            return "INFERRED     推得，剩 \((end - now) / 60000) 分"
        case .unavailable: return "UNAVAILABLE  無法判定"
        }
    }
}
