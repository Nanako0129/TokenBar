import Foundation
import TokenBarCore

/// CLI smoke flow (Phase 1): exercise every FFI entry point and print a
/// one-line summary each. Kept behind `--smoke` so CI can validate the
/// bridge without booting the menu-bar app.
enum Smoke {
    /// Runs every check and returns the process exit code (0 = all green).
    /// Per-provider quota errors inside `agentUsage` print as card errors and
    /// do not fail the run; only thrown errors count as failures.
    static func run() -> Int32 {
        var failures = 0

        func summarize(_ label: String, _ body: () throws -> String) {
            do {
                print("\(label): \(try body())")
            } catch {
                failures += 1
                print("\(label): FAILED — \(error)")
            }
        }

        summarize("probe") {
            let probe = try TBCore.probe()
            return "\(probe.messages ?? 0) parsed local messages"
        }

        summarize("graph") {
            let graph = try TBCore.graph()
            return "\(graph.contributions.count) days, total tokens \(graph.summary.totalTokens), "
                + "cost $\(String(format: "%.2f", graph.summary.totalCost)), "
                + "\(graph.summary.clients.count) clients, \(graph.years.count) years"
        }

        summarize("refreshGraph(2026)") {
            let graph = try TBCore.refreshGraph(year: "2026")
            return "\(graph.contributions.count) days, range \(graph.meta.dateRange.start)..\(graph.meta.dateRange.end)"
        }

        summarize("models") {
            let report = try TBCore.modelReport()
            let top = report.entries.max(by: { $0.cost < $1.cost })
            return "\(report.entries.count) models, \(report.totalMessages) messages, "
                + "top=\(top.map { "\($0.model) ($\(String(format: "%.2f", $0.cost)))" } ?? "none"), "
                + "pricesUpdated=\(report.pricingUpdatedAt.map(String.init) ?? "nil")"
        }

        summarize("hourly") {
            let report = try TBCore.hourlyReport()
            return "\(report.entries.count) slots, total cost $\(String(format: "%.2f", report.totalCost))"
        }

        summarize("agents") {
            let report = try TBCore.agentsReport()
            let top = report.entries.first // pre-sorted by cost desc
            return "\(report.entries.count) agents, \(report.totalMessages) messages, "
                + "top=\(top.map(\.agent) ?? "none")"
        }

        summarize("trace") {
            let buckets = try TBCore.usageTrace(windowSecs: 600)
            let rate = try TBCore.tokensPerMin()
            return "\(buckets.count) buckets (10m window), tokens/min \(String(format: "%.1f", rate))"
        }

        // Drift probe (issue #35): force trayTotals' slow re-sum path over the
        // REAL payload (a hidden id that matches no client excludes nothing) and
        // compare to the FFI summary. Print-only — a mismatch never fails the
        // run; it flags a vendor-sync regression in the aggregator's clamp
        // granularity (see UsagePayload.trayTotals' doc comment).
        summarize("trayDrift") {
            let graph = try TBCore.graph()
            let totals = graph.trayTotals(hidden: ["__none__"], today: Format.todayKey())
            let tokenMatch = totals.totalTokens == graph.summary.totalTokens
            let costMatch = abs(totals.totalCost - graph.summary.totalCost) < 0.01
            let status = tokenMatch && costMatch ? "match" : "MISMATCH"
            return "\(status) — reSum \(totals.totalTokens) tok / "
                + "$\(String(format: "%.2f", totals.totalCost)) vs summary "
                + "\(graph.summary.totalTokens) tok / $\(String(format: "%.2f", graph.summary.totalCost))"
        }

        summarize("agentUsage") {
            let usage = try TBCore.agentUsage()
            let cards = usage.agents.map { snapshot in
                if let error = snapshot.error {
                    return "\(snapshot.clientId)=error(\(error))"
                }
                return "\(snapshot.clientId)=\(snapshot.windows.count) windows"
            }
            let subs = usage.opencodeSubscriptions ?? []
            return cards.joined(separator: ", ")
                + (subs.isEmpty ? "" : " | opencode subs: \(subs.joined(separator: ", "))")
        }

        if failures > 0 {
            print("\(failures) entry point(s) failed")
            return 1
        }
        return 0
    }
}
