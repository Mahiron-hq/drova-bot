# Changelog

All notable changes to this project are documented in this file. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-11-26

First public release. The bot was rewritten from Python to Rust.

### Added

- Current game of busy stations in `/status` and `/list`; game of the last session in notifications, station cards and `/history`; session length in "free" notifications.
- Station card in `/list` with history, comment, share link, removal and a link to drova.io.
- Merchant alerts per station: "session started" with the game and "station went offline".
- Merchant and admin statistics for 7 days: load, sessions, idle time, peak hours, top games.
- Quiet hours in the user's time zone: messages arrive without sound.
- Time zone setting; all times are shown in the user's zone.
- `/settings` menu, `/about`, `/privacy`, `/forget`, admin `/broadcast`.
- One-click subscribe links `t.me/<bot>?start=<station uuid>`.
- Export and import keep comments.
- Higher subscription limit for merchants (20 by default, configurable).
- Admin alert when the Drova API is down as a whole and when Drova returns an unknown station state.
- systemd watchdog, readiness and clean shutdown.
- Test mode `ALLOWED_CHAT_IDS`.
- `migrate-legacy` command that converts the database of the Python bot.

### Changed

- New database schema with Unix timestamps, one row per station and automatic versioned migrations. Old column names stay available through read-only views.
- All messages go through one persistent queue that respects Telegram limits; stale "free" notifications are dropped instead of being sent late.
- `/status` answers from the monitor cache and shows how fresh the data is.
- Polling interval is 30 seconds by default.
- Users who block the bot keep their data for 90 days and get it back when they return.
- Messages use HTML formatting with escaping of all dynamic text.
- Shorter command menu; old commands keep working.
- Retention: sent notifications 30 days, station history 90 days.

### Fixed

- Users waiting for a hidden station were not notified when it became public.
- Muted users got a duplicate "free" message right after the silence ended.
- Admin alerts could pile up and hit Telegram flood limits.
