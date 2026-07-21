## Fixes

- **Opening the popover no longer crashes the app.** Some users updating to v1.6.1 could see TokenBar quit immediately when opening the popover. Height synchronization now waits until the current layout is complete. [#80](https://github.com/Nanako0129/TokenBar/pull/80)
- **Quota bars keep a consistent thickness.** Rows without a pace marker no longer appear thinner than rows with one.
