## Highlights

- **TokenBar now includes Traditional Chinese.** Choose System, English, or Traditional Chinese in Settings; when the selection changes, TokenBar offers to restart immediately so the new preference can take effect. Dashboards, settings, menus, charts, quota reset countdowns, usage pace and risk text, and graph controls are localized, with English fallback for missing entries. [#106](https://github.com/Nanako0129/TokenBar/pull/106) [#108](https://github.com/Nanako0129/TokenBar/pull/108)

## Fixes

- **Copilot quota and activity data now withstand more imperfect inputs.** An optional reset date with an unexpected type no longer discards otherwise valid quota windows, and duplicate OTEL spans keep the earliest start through the latest endpoint without adding replayed token usage. [#102](https://github.com/Nanako0129/TokenBar/pull/102)
- **Kimi Code discovery now treats an empty `KIMI_CODE_HOME` as unset,** returning to the default Kimi Code directory instead of scanning an invalid root. Non-empty overrides keep their existing behavior. [#101](https://github.com/Nanako0129/TokenBar/pull/101)
- **Provider connection errors now retain typed DNS and TLS diagnostics without exposing raw transport text.** Existing timeout and connection-failure precedence remains unchanged. [#102](https://github.com/Nanako0129/TokenBar/pull/102)
