## Before you update

**Reported costs move up, reported tokens move down, and neither is a change to what you actually spent.** Four separate pricing corrections land together.

Three of them are repricings, measured over the same local corpus of 1,144 Claude transcripts.

- **One-hour prompt-cache writes were billed at the five-minute rate.** [#287](https://github.com/Nanako0129/TokenBar/issues/287)

  Anthropic charges 1.25× base input for a five-minute cache write and 2× for a one-hour one, and the transcript reports both under a single total. TokenBar read only that total and priced all of it at the cheaper rate. It now reads the split. On the measured corpus the one-hour TTL accounts for 79.6% of cache-write tokens and total cost rises **7.18%** — TokenBar stopped under-charging rather than starting to over-charge. **Token counts are unchanged.** Only the price attached to them moved.

- **`tool_result` text was counted twice.** [#288](https://github.com/Nanako0129/TokenBar/issues/288) — thanks [@Mai0313](https://github.com/Mai0313)

  The parser estimated input tokens for a tool result at one per four characters, and then the next assistant turn reported the same text again in its own usage. Reported Claude input tokens fall, **by an amount that varies enormously with how tool-heavy the usage is**: −5.9% over the 1,144-transcript corpus, −91.9% over a contributor's 6,219. Output, cache reads and cache writes are byte-identical in both.

- **Codex reasoning tokens were priced twice.** [#289](https://github.com/Nanako0129/TokenBar/pull/289)

  Historical Codex figures fall in proportion to reasoning effort — about 6% on one local corpus, more for an account that runs high effort throughout. Nothing was deleted. The same tokens are counted once instead of twice.

The fourth is not a repricing at all.

- **A historical cost could change on its own, and now cannot.** [#300](https://github.com/Nanako0129/TokenBar/issues/300) — thanks [@huyanxius](https://github.com/huyanxius)

  When several pricing entries matched a model equally well, the one used was whichever the lookup table happened to iterate first, and that order changed every time the table reloaded. The same usage could be priced one way today and another tomorrow with nothing new behind it. Two hundred runs against a fixed table returned two different answers — 117 runs one way, 83 the other. Ties are now broken deterministically, so an affected model settles once and stops moving.

**The first launch after updating re-scans every transcript once.** The cache format changed, and only Claude's entries can be carried across it, so every other agent's cached parse is rebuilt from source. On 3.0 GB of Codex logs and 2.4 GB of Claude logs that took 18–28 seconds, against 1–4 seconds warm. It happens once. The dashboard also opens on a loading state rather than the previous chart, for that launch only.

Claude's history survives the rebuild instead of being discarded — including turns a compaction has already removed from the transcript, where the cache is the only remaining copy. This is the first cache-format change that migrates rather than dropping.

## Fixes

- **The two Codex Spark quota windows were both called "Codex Spark".** [#286](https://github.com/Nanako0129/TokenBar/issues/286) — thanks [@Agugu-official](https://github.com/Agugu-official)

  Codex reports its five-hour and seven-day Spark limits under one name, and TokenBar named each window after the limit it came from. The window picker, the limits card, the Settings menu and the menu-bar source list each offered two entries that could not be told apart.

  Each window is now qualified by evidence it actually carries: its length where that is known — Codex Spark · Session and Codex Spark · Weekly — otherwise its own reset time, otherwise a number. Nothing that was already unique is renamed; a window with a single label reads exactly as it did.

- **Codex windows with no usage yet showed no pace and no duration.** [#296](https://github.com/Nanako0129/TokenBar/issues/296)

  A provider's reply was checked against a clock read before the request went out, so the evidence was always one round trip stale. For a window Codex reports as unused, the reset it returns sits a full window ahead of *its* clock, which put the window's start in the future of that stale reading — by about a second, which was enough. The reply was rejected as invalid, and the duration, window length and pace went with it. Those rows read "No pace data · quota data invalid".

  The clock is now read after the reply arrives. Windows that were blank get their pace back, and start accumulating quota history from here.

- **Window history stayed on "Reading local usage…" indefinitely.** [#293](https://github.com/Nanako0129/TokenBar/pull/293)

  The history card scanned a range anchored to the live window. When the provider stopped supplying a duration — the state the fix above describes — there was no anchor, the scan returned nothing, and every row sat on its loading message. The cycle itself now counts as a valid range. The two problems are independent, so this holds even if a provider degrades again.
