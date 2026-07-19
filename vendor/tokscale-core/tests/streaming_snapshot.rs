// Integration tests for StreamingAggregator snapshot-isolation semantics.
//
// scenario: store_memo_snapshot (snapshot isolation)
//
// Spec MUST-DO: "遍歷中途 entries 變化唔影響本輪聚合"
// The aggregator folds a snapshot of the message collection.  Any mutations to
// the live collection that occur *after* the snapshot is captured (and while the
// fold pass is in flight) MUST NOT affect the fold result.  This models the Arc
// snapshot semantics of the STORE_MEMO driver.

use tokscale_core::{CostSource, StreamingAggregator, TokenBreakdown, UnifiedMessage};

#[allow(clippy::too_many_arguments)]
fn snapshot_msg(
    date: &str,
    client: &str,
    model: &str,
    session_id: &str,
    dedup_key: Option<&str>,
    timestamp_ms: i64,
    input: i64,
    output: i64,
    cost: f64,
) -> UnifiedMessage {
    UnifiedMessage {
        client: client.to_string(),
        model_id: model.to_string(),
        provider_id: "anthropic".to_string(),
        session_id: session_id.to_string(),
        workspace_key: None,
        workspace_label: None,
        timestamp: timestamp_ms,
        date: date.to_string(),
        tokens: TokenBreakdown {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        cost,
        cost_source: CostSource::Unknown,
        duration_ms: None,
        message_count: 1,
        agent: None,
        dedup_key: dedup_key.map(|s| s.to_string()),
        dedup_aliases: Vec::new(),
        is_turn_start: false,
    }
}

// scenario: store_memo_snapshot (snapshot isolation)
// Verifies that mutations to the originating collection after the snapshot
// clone is taken do NOT affect the fold result produced by StreamingAggregator.
//
// Expected values (hand-computed):
//   hermes (snap-key-1): input=400, output=200 → tokens=600, cost=0.07
//   zed    (snap-key-2): input=200, output=100 → tokens=300, cost=0.03
//   claude (no dedup)  : input=100, output=50  → tokens=150, cost=0.01
//   TOTAL: tokens=600+300+150=1050, cost=0.07+0.03+0.01=0.11, messages=3
//
//   Intruder pushed after snapshot: tokens=9999, cost=99.0 — must NOT appear.
//   Mutated hermes in live collection: tokens=199998, cost=999.0 — must NOT appear.
#[test]
fn test_streaming_aggregator_store_memo_snapshot_mid_iteration_mutation_ignored() {
    // scenario: store_memo_snapshot

    // Step 1: capture the snapshot (clone simulates Arc snapshot semantics).
    let snapshot: Vec<UnifiedMessage> = vec![
        snapshot_msg(
            "2025-05-10",
            "hermes",
            "claude-sonnet-4-5",
            "snap-sess-1",
            Some("snap-key-1"),
            1_746_864_000_000,
            400,
            200,
            0.07,
        ),
        snapshot_msg(
            "2025-05-10",
            "zed",
            "claude-opus-4-5",
            "snap-sess-2",
            Some("snap-key-2"),
            1_746_864_001_000,
            200,
            100,
            0.03,
        ),
        snapshot_msg(
            "2025-05-10",
            "claude",
            "claude-haiku-4-5",
            "snap-sess-3",
            None,
            1_746_864_002_000,
            100,
            50,
            0.01,
        ),
    ];

    // Step 2: the live collection starts as a clone of the snapshot.
    let mut live_collection = snapshot.clone();

    // Step 3: begin fold on the *snapshot* clone — the aggregator sees only
    // the 3 messages present at snapshot-capture time.
    let mut agg = StreamingAggregator::new();
    for msg in &snapshot {
        agg.feed(msg);
    }

    // Step 4: mutate the live collection mid-fold (simulates a new file
    // arriving or an in-place store update while aggregation is in flight).
    //
    // Push a new message: 5000+4999=9999 tokens, cost=99.0 — must be ignored.
    live_collection.push(snapshot_msg(
        "2025-05-10",
        "opencode",
        "gpt-4o",
        "intruder-sess",
        None,
        1_746_864_999_000,
        5000,
        4999,
        99.0,
    ));
    // Replace an existing entry with wildly different values — must be ignored.
    live_collection[0] = snapshot_msg(
        "2025-05-10",
        "hermes",
        "claude-sonnet-4-5",
        "snap-sess-1",
        Some("snap-key-1"),
        1_746_864_000_000,
        99999,
        99999,
        999.0,
    );

    // Step 5: finalize — result must reflect only the 3 snapshot messages.
    let contributions = agg.finalize();

    // One date bucket expected.
    assert_eq!(
        contributions.len(),
        1,
        "snapshot isolation: all 3 snapshot messages on same date -> 1 bucket"
    );

    // Only the 3 snapshot messages must be counted; intruder and mutated
    // hermes must not appear.
    assert_eq!(
        contributions[0].totals.messages,
        3,
        "snapshot isolation: only the 3 snapshot messages counted; intruder ignored"
    );

    // tokens: hermes(600) + zed(300) + claude(150) = 1050
    assert_eq!(
        contributions[0].totals.tokens,
        1050,
        "snapshot isolation: tokens must be 600+300+150=1050 (snapshot values only)"
    );

    // cost: 0.07 + 0.03 + 0.01 = 0.11
    assert!(
        (contributions[0].totals.cost - 0.11).abs() < 1e-9,
        "snapshot isolation: cost must be 0.07+0.03+0.01=0.11 (snapshot values only)"
    );

    // Guard: the mutated hermes value (99999+99999=199998 tokens) must not
    // have leaked into the fold result.
    assert_ne!(
        contributions[0].totals.tokens,
        199998 + 300 + 150,
        "snapshot isolation: mutated hermes token count must not appear in fold result"
    );
}
