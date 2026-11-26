<div align="center">

# Drova Bot

**Telegram bot that watches Drova cloud gaming stations and tells you when they become free**
<br>
**Telegram-бот, который следит за станциями облачного гейминга Drova и сообщает, когда они освобождаются**

[![Release](https://img.shields.io/github/v/release/Mahiron-hq/drova-bot?style=flat-square)](https://github.com/Mahiron-hq/drova-bot/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/Mahiron-hq/drova-bot/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/Mahiron-hq/drova-bot/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-blue?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20server-555?style=flat-square)](#install)
[![Rust](https://img.shields.io/badge/rust-1.99-orange?style=flat-square)](https://www.rust-lang.org)

### [Open the bot / Открыть бота](https://t.me/drova_servers_bot) | [Download / Скачать](https://github.com/Mahiron-hq/drova-bot/releases/latest)

**[English](#english)** | **[Русский](#russian)**

</div>

| | |
|---|---|
| Bot | [@drova_servers_bot](https://t.me/drova_servers_bot) |
| Platform | Linux x86_64 (static binary), any systemd distribution |
| License | GPL-3.0-or-later |
| Download | [Releases](https://github.com/Mahiron-hq/drova-bot/releases/latest) |
| Language | Russian (bot interface) |

---

## English

Drova Bot watches stations of the [Drova](https://drova.io) cloud gaming service and sends a Telegram message the moment a station you follow becomes free. You subscribe by sending a station link; the bot polls the public Drova API and keeps a history of every station.

The bot is unofficial and not affiliated with Drova. It uses only public station data.

### Features

- Notification when a station goes from "busy" to "free" or comes back from offline, with the game of the last session and how long it lasted.
- If the station is taken again within 5 minutes, the notification updates itself ("already taken, playing X").
- Two roles. Users get notified when a station is free and public. Merchants also get notified when a station is free but hidden, can turn on "session started" and "went offline" alerts per station and see 7-day statistics: load, sessions, idle time, peak hours and top games.
- 24-hour history of status changes with games for everyone.
- Silence for 3, 8 or 24 hours or until turned off, quiet hours in your own time zone (messages arrive without sound), comments on stations, export and import of subscriptions.
- One-click subscribe links (`t.me/drova_servers_bot?start=<station uuid>`) that a merchant can put into the station description.
- Reliable delivery: a persistent queue that respects Telegram limits, retries on network errors, and drops stale "free" messages instead of sending them late.
- Built for a server: one static binary, SQLite with automatic migrations, systemd watchdog, clean shutdown, data retention limits and admin alerts when the Drova API fails.

### Install

Requirements: Linux x86_64 with systemd, a bot token from [@BotFather](https://t.me/BotFather).

1. Download `drova-bot-<version>-linux-x86_64.tar.gz` and `SHA256SUMS.txt` from [Releases](https://github.com/Mahiron-hq/drova-bot/releases/latest) and check the hash: `sha256sum -c SHA256SUMS.txt --ignore-missing`.
2. Unpack and install the binary: `sudo install -m 755 drova-bot /usr/local/bin/drova-bot`.
3. Create a user and a data folder: `sudo useradd --system --home /var/lib/drova-bot drova-bot && sudo install -d -o drova-bot -m 750 /var/lib/drova-bot`.
4. Copy [`deploy/drova-bot.env.example`](deploy/drova-bot.env.example) to `/etc/drova-bot/drova-bot.env`, set `BOT_TOKEN` and `ADMIN_IDS`, and make the file readable only by root (`chmod 600`).
5. Install [`deploy/drova-bot.service`](deploy/drova-bot.service) into `/etc/systemd/system/` and run `sudo systemctl enable --now drova-bot`.

Logs: `journalctl -u drova-bot -f`. Backups: [`deploy/drova-backup`](deploy/drova-backup) with its timer makes a daily consistent copy of the database.

### Configuration

All settings come from environment variables (or a `.env` file in the working directory).

| Variable | Default | Meaning |
|---|---|---|
| `BOT_TOKEN` | required | Telegram bot token |
| `DATABASE_PATH` | `drova_monitor.sqlite3` | SQLite database file |
| `ADMIN_IDS` | empty | Telegram IDs of admins, comma separated |
| `POLL_INTERVAL_SECONDS` | `30` | Pause between polling rounds |
| `POLL_CONCURRENCY` | `5` | Parallel requests to the Drova API |
| `REQUEST_DELAY_SECONDS` | `0.7` | Pause after each request |
| `HTTP_TIMEOUT_SECONDS` | `15` | Drova API timeout |
| `MAX_SUBSCRIPTIONS_USER` | `5` | Subscription limit for users |
| `MAX_SUBSCRIPTIONS_MERCHANT` | `20` | Subscription limit for merchants |
| `DONATE_TEXT_FILE` | empty | Telegram HTML file shown by `/donate` |
| `WEEKLY_REMINDER_FILE` | empty | Telegram HTML file sent on Mondays at 19:00 Moscow time |
| `ALLOWED_CHAT_IDS` | empty | Test mode: the bot sends nothing outside these chats |
| `LOG_LEVEL` | `INFO` | `ERROR`, `WARN`, `INFO`, `DEBUG` |

### Moving from the old Python bot

The new database has a cleaner schema. Convert the old file once, while the old bot is stopped:

```sh
drova-bot migrate-legacy drova_monitor.old.sqlite3 drova_monitor.sqlite3
```

The old file is not changed. Users, roles, silence, subscriptions, comments, status history of the last 90 days and the weekly reminder state are moved. Read-only views `monitoring`, `user_settings` and `server_comments` keep the old column names for external tools.

### Build from source

```sh
cargo build --release
cargo test
```

Release binaries are built by GitHub Actions as static `x86_64-unknown-linux-musl` executables.

### Privacy and security

The bot stores only Telegram IDs, subscriptions, comments and settings. See [PRIVACY.md](PRIVACY.md) and [SECURITY.md](SECURITY.md).

### Changelog

See [CHANGELOG.md](CHANGELOG.md).

If the bot is useful to you, a star on GitHub helps the project.

---

## Russian

Drova Bot следит за станциями сервиса облачного гейминга [Drova](https://drova.io) и присылает сообщение в Telegram, как только станция, на которую вы подписаны, освобождается. Подписка оформляется отправкой ссылки на станцию. Бот опрашивает публичное API Drova и хранит историю каждой станции.

Бот неофициальный и не связан с Drova. Он использует только публичные данные о станциях.

### Возможности

- Уведомление, когда станция переходит из «занят» в «свободна» или возвращается из офлайна, с игрой последней сессии и её длительностью.
- Если станцию займут в течение 5 минут, уведомление само обновится («уже заняли, играют в X»).
- Две роли. Пользователь получает уведомление, когда станция свободна и открыта для всех. Мерчант дополнительно получает уведомление, когда станция свободна, но скрыта, может включить для каждой станции уведомления «станцию заняли» и «станция ушла в офлайн» и видит статистику за 7 дней: загрузку, сессии, простой, пиковые часы и топ игр.
- История смен статуса за 24 часа с играми для всех.
- Тишина на 3, 8, 24 часа или до отмены, тихие часы в вашем часовом поясе (сообщения без звука), комментарии к станциям, экспорт и импорт подписок.
- Ссылки для подписки в один клик (`t.me/drova_servers_bot?start=<uuid станции>`), которые мерчант может добавить в описание станции.
- Надёжная доставка: постоянная очередь с учётом лимитов Telegram, повторы при сетевых ошибках, устаревшие сообщения «свободна» не отправляются с опозданием.
- Рассчитан на сервер: один статический бинарник, SQLite с автоматическими миграциями, watchdog systemd, корректная остановка, сроки хранения данных и оповещения админа при сбоях API Drova.

### Установка

Требования: Linux x86_64 с systemd, токен бота от [@BotFather](https://t.me/BotFather).

1. Скачайте `drova-bot-<версия>-linux-x86_64.tar.gz` и `SHA256SUMS.txt` из [релизов](https://github.com/Mahiron-hq/drova-bot/releases/latest) и проверьте хеш: `sha256sum -c SHA256SUMS.txt --ignore-missing`.
2. Распакуйте и установите бинарник: `sudo install -m 755 drova-bot /usr/local/bin/drova-bot`.
3. Создайте пользователя и папку данных: `sudo useradd --system --home /var/lib/drova-bot drova-bot && sudo install -d -o drova-bot -m 750 /var/lib/drova-bot`.
4. Скопируйте [`deploy/drova-bot.env.example`](deploy/drova-bot.env.example) в `/etc/drova-bot/drova-bot.env`, укажите `BOT_TOKEN` и `ADMIN_IDS` и оставьте файл доступным только root (`chmod 600`).
5. Установите [`deploy/drova-bot.service`](deploy/drova-bot.service) в `/etc/systemd/system/` и выполните `sudo systemctl enable --now drova-bot`.

Логи: `journalctl -u drova-bot -f`. Бэкапы: [`deploy/drova-backup`](deploy/drova-backup) с таймером каждый день делает целостную копию базы.

### Настройки

Все настройки задаются переменными окружения (или файлом `.env` в рабочей папке), полный список в английском разделе выше. Главные: `BOT_TOKEN`, `DATABASE_PATH`, `ADMIN_IDS`, `POLL_INTERVAL_SECONDS`, лимиты подписок `MAX_SUBSCRIPTIONS_USER` и `MAX_SUBSCRIPTIONS_MERCHANT`, тексты `DONATE_TEXT_FILE` и `WEEKLY_REMINDER_FILE`, тестовый режим `ALLOWED_CHAT_IDS`.

### Переход со старого Python-бота

У новой базы более чистая схема. Старый файл нужно один раз конвертировать, пока старый бот остановлен:

```sh
drova-bot migrate-legacy drova_monitor.old.sqlite3 drova_monitor.sqlite3
```

Старый файл не меняется. Переносятся пользователи, роли, тишина, подписки, комментарии, история статусов за последние 90 дней и состояние еженедельной рассылки. Представления `monitoring`, `user_settings` и `server_comments` сохраняют старые имена колонок для внешних инструментов.

### Сборка из исходников

```sh
cargo build --release
cargo test
```

Бинарники релизов собирает GitHub Actions как статические исполняемые файлы `x86_64-unknown-linux-musl`.

### Конфиденциальность и безопасность

Бот хранит только Telegram ID, подписки, комментарии и настройки. Подробнее в [PRIVACY.md](PRIVACY.md) и [SECURITY.md](SECURITY.md).

### История изменений

См. [CHANGELOG.md](CHANGELOG.md).

Если бот вам полезен, поставьте звезду репозиторию на GitHub, это помогает проекту.

---

## License

GPL-3.0-or-later. Copyright © 2026 Lev Burmistrov (Mahiron-hq).
Contact: Mahiron.HQ@protonmail.com | t.me/mahiron_hq
