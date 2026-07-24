## Highlights

- **Animated status icons now use far less TokenBar CPU.** In controlled 40 FPS benchmarks, TokenBar process CPU fell from 14.938% to 0.458% for Cat and from 10.287% to 0.462% for Parrot while preserving the configured cadence. Visible animation still carries WindowServer composition cost. [#95](https://github.com/Nanako0129/TokenBar/pull/95)
- **Quota cards now survive transient provider outages without hiding the error.** TokenBar keeps the last known windows for the same account and shows the current Error. Sign-outs, account changes, authentication failures, and invalid responses still clear stale data instead of reusing it. [#94](https://github.com/Nanako0129/TokenBar/pull/94)

## Changes

- **Usage cache validation now follows related files more precisely.** TokenBar invalidates cached source data when dependencies move, appear, or disappear. The first scan after updating starts cold, then later refreshes use the rebuilt cache. [#91](https://github.com/Nanako0129/TokenBar/pull/91)

## Fixes

- **Local data discovery remains reliable when GUI launches lack a full shell environment.** Explicit source overrides keep their existing precedence. [#92](https://github.com/Nanako0129/TokenBar/pull/92)
- **Claude version detection now runs at most once per TokenBar launch,** avoiding repeated CLI starts during quota refreshes. [#93](https://github.com/Nanako0129/TokenBar/pull/93)
