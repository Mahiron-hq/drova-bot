<div align="center">

# Drova Bot

**Telegram bot that watches Drova cloud gaming stations and tells you when they become free**
<br>
**Telegram-бот, который следит за станциями облачного гейминга Drova и сообщает, когда они освобождаются**

[![Release](https://img.shields.io/github/v/release/Mahiron-hq/drova-bot?style=flat-square)](https://github.com/Mahiron-hq/drova-bot/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/Mahiron-hq/drova-bot/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/Mahiron-hq/drova-bot/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-blue?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20server-555?style=flat-square)](#installation)
[![Rust](https://img.shields.io/badge/rust-1.99-orange?style=flat-square)](https://www.rust-lang.org)

### [Open the bot / Открыть бота](https://t.me/drova_servers_bot) · [Download / Скачать](https://github.com/Mahiron-hq/drova-bot/releases/latest)

**[English](#english)** · **[Русский](#русский)**

</div>

| | |
|---|---|
| Bot | [@drova_servers_bot](https://t.me/drova_servers_bot) |
| Platform | Linux x86_64 server (static binary), any systemd distribution |
| License | GPL-3.0-or-later |
| Download | [Releases](https://github.com/Mahiron-hq/drova-bot/releases/latest) |
| Language | Russian (bot interface) |

---

## English

Drova Bot watches stations of the [Drova](https://drova.io) cloud gaming service and sends a Telegram message the moment a station you follow becomes free. You subscribe by sending a station link; the bot polls the public Drova API every 30 seconds and keeps a history of every station.

The bot is for players who wait for a particular station and for merchants who run stations and want to know what happens to them. To use it, just open [@drova_servers_bot](https://t.me/drova_servers_bot); the rest of this page is mostly for those who want to run their own copy.

### Contents

- [Why Drova Bot](#why-drova-bot)
- [What the bot can do](#what-the-bot-can-do)
- [Version 2 compared with the Python bot](#version-2-compared-with-the-python-bot)
- [How to use](#how-to-use)
- [Installation](#installation)
- [Configuration](#configuration)
- [Limitations](#limitations)
- [Privacy, license and rights](#privacy)

### Why Drova Bot

#### 1. You learn about a free station first

A popular station is free for a minute or two. The bot checks every station with subscribers every 30 seconds and sends the message right away. If somebody takes the station within 5 minutes, the message edits itself to "already taken, playing X", so you do not open a station that is busy again.

#### 2. The states match drova.io

Station states are read the same way the site reads them: "ready", "busy" (including a client that is still connecting) and "offline". The game shown for a busy station is the one being played now; after the session it becomes the game of the last session. The rules were checked against the site code and three hours of observing real stations.

#### 3. Everything about a station in one card

The card shows the status and for how long, the current or last game and the session length, city, number of games, hardware (processor, video cards, memory), the description from drova.io and your own comment. From the card you open the history, share a one-click subscribe link, or remove the station.

#### 4. Tools for merchants

Merchants get a message when a station is free but still hidden from players, can turn on "session started" and "went offline" alerts per station, and see 7-day statistics: load, sessions, idle time, peak hours and top games.

#### 5. Made for a server

One static binary without OpenSSL, SQLite with automatic versioned migrations and a backup before each migration, a systemd watchdog, a persistent message queue that respects Telegram limits, and admin alerts when the Drova API fails.

#### 6. Open source and little data

The source code is open under GPL-3.0-or-later. The bot stores only your Telegram ID, subscriptions, comments and settings, and `/forget` deletes all of it at once.

### What the bot can do

- **Notifications:** a station became free or came back online, with the game of the last session and how long it lasted; the message updates itself if the station is taken again within 5 minutes.
- **Status and list:** `/status` and `/list` show every subscription with an icon, the waiting count and the current game of busy stations. The data is never older than one polling round.
- **Station card:** history, comment, share link, removal, a link to drova.io, hardware and description.
- **History:** status changes for the last 24 hours with games for everyone; 7-day statistics for merchants and the admin.
- **Deleted stations:** if Drova answers "not found" for an hour, the bot asks whether to remove the station. A deleted station is checked once an hour; if it comes back, you get a message; if not, the subscription is removed after 30 days.
- **Quiet time:** silence for 3, 8 or 24 hours or until turned off, and quiet hours in your own time zone, when messages arrive without sound.
- **Subscriptions:** up to 5 for players and 20 for merchants by default, export and import with comments, one-click links `t.me/drova_servers_bot?start=<station uuid>` that a merchant can put into the station description.
- **For the admin:** statistics, recent errors, a broadcast with confirmation, an alert when the Drova API is down or returns an unknown station state.

### Version 2 compared with the Python bot

Version 1 was written in Python; its code is kept in the [`python`](https://github.com/Mahiron-hq/drova-bot/tree/python) branch and is no longer updated.

| | 1.0 (Python) | 2.0 (Rust) |
|---|---|---|
| Polling interval | 60 seconds | 30 seconds |
| Game of the session | No | Current game in lists, last game in notifications and the card |
| Station card | No | Status, game, hardware, description, history, sharing |
| Message when a station is taken again | "Already taken" mark | The same mark with the game being played |
| Merchant alerts | Hidden free stations | Also "session started" and "went offline" per station |
| Merchant statistics for 7 days | Average session and idle time | Also load, number of sessions, peak hours and top games |
| Quiet hours and time zone | No | Yes |
| Deleted stations | Checked forever | Question to subscribers, hourly checks, removal after 30 days |
| Message delivery | Direct sending | Persistent queue with Telegram limits and retries |
| Database | Original schema | Versioned migrations; the old database is converted without loss |
| Running on a server | Python with dependencies | One static binary, systemd watchdog |

### How to use

1. Open [@drova_servers_bot](https://t.me/drova_servers_bot) and press "Start".
2. Choose a role: player or merchant. It can be changed later with `/role`.
3. Send a station link like `https://drova.io/stations/<uuid>`. You can send several links in one message.
4. Wait for the message. `/status` shows all stations at once, `/list` opens the cards.
5. Settings, silence and quiet hours are in `/settings`, full help is in `/help`.

### Installation

This is needed only to run your own copy of the bot. Requirements: Linux x86_64 with systemd and a bot token from [@BotFather](https://t.me/BotFather).

1. Download `drova-bot-<version>-linux-x86_64.tar.gz` and `SHA256SUMS.txt` from [Releases](https://github.com/Mahiron-hq/drova-bot/releases/latest) and check the hash: `sha256sum -c SHA256SUMS.txt --ignore-missing`.
2. Unpack and install the binary: `sudo install -m 755 drova-bot /usr/local/bin/drova-bot`.
3. Create a user and a data folder: `sudo useradd --system --home /var/lib/drova-bot drova-bot && sudo install -d -o drova-bot -m 750 /var/lib/drova-bot`.
4. Copy [`deploy/drova-bot.env.example`](deploy/drova-bot.env.example) to `/etc/drova-bot/drova-bot.env`, set `BOT_TOKEN` and `ADMIN_IDS`, and make the file readable only by root (`chmod 600`).
5. Install [`deploy/drova-bot.service`](deploy/drova-bot.service) into `/etc/systemd/system/` and run `sudo systemctl enable --now drova-bot`.

Logs: `journalctl -u drova-bot -f`. Backups: [`deploy/drova-backup`](deploy/drova-backup) with its timer makes a daily consistent copy of the database.

#### Moving from the Python bot

Convert the old database once, while the old bot is stopped:

```sh
drova-bot migrate-legacy drova_monitor.old.sqlite3 drova_monitor.sqlite3
```

The old file is not changed. Users, roles, silence, subscriptions, comments, status history of the last 90 days and the weekly reminder state are moved. Read-only views `monitoring`, `user_settings` and `server_comments` keep the old column names for external tools.

#### Build from source

```sh
cargo build --release
cargo test
```

Release binaries are built by GitHub Actions as static `x86_64-unknown-linux-musl` executables.

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
| `MAX_SUBSCRIPTIONS_USER` | `5` | Subscription limit for players |
| `MAX_SUBSCRIPTIONS_MERCHANT` | `20` | Subscription limit for merchants |
| `DONATE_TEXT_FILE` | empty | Telegram HTML file shown by `/donate` |
| `WEEKLY_REMINDER_FILE` | empty | Telegram HTML file sent on Mondays at 19:00 Moscow time |
| `ALLOWED_CHAT_IDS` | empty | Test mode: the bot sends nothing outside these chats |
| `LOG_LEVEL` | `INFO` | `ERROR`, `WARN`, `INFO`, `DEBUG` |

### Limitations

- The bot uses only the public Drova API. Session history, earnings and other data of a merchant account are not available to it.
- A status change reaches you within one polling round (30 seconds by default) plus Telegram delivery time.
- While a client is connecting (up to 2-3 minutes), the station is already shown as busy, even if the connection fails later.
- The bot interface is in Russian only.

### Privacy

The bot stores only Telegram IDs, subscriptions, comments and settings, keeps sent messages for 30 days and station history for 90 days. Details: [PRIVACY.md](PRIVACY.md).

### Disclaimer

The bot is provided "as is", without warranty. A message may come late or not at all if Drova or Telegram are unavailable.

Drova Bot is an independent open source project. It is not affiliated with or endorsed by Drova. The name Drova is used only to indicate what the bot works with.

### License

GPL-3.0-or-later, see [LICENSE](LICENSE). Reporting a vulnerability: [SECURITY.md](SECURITY.md). Changes: [CHANGELOG.md](CHANGELOG.md). The Python version: [`python`](https://github.com/Mahiron-hq/drova-bot/tree/python) branch.

If the bot is useful to you, a star on GitHub helps the project.

---

## Русский

Drova Bot следит за станциями сервиса облачного гейминга [Drova](https://drova.io) и присылает сообщение в Telegram, как только станция, на которую вы подписаны, освобождается. Подписка оформляется отправкой ссылки на станцию. Бот опрашивает публичное API Drova каждые 30 секунд и хранит историю каждой станции.

Бот нужен игрокам, которые ждут конкретную станцию, и мерчантам, которые держат станции и хотят знать, что с ними происходит. Чтобы пользоваться ботом, достаточно открыть [@drova_servers_bot](https://t.me/drova_servers_bot). Остальная часть страницы в основном для тех, кто хочет запустить свою копию.

### Содержание

- [Почему Drova Bot](#почему-drova-bot)
- [Что умеет бот](#что-умеет-бот)
- [Версия 2 в сравнении с Python-ботом](#версия-2-в-сравнении-с-python-ботом)
- [Как пользоваться](#как-пользоваться)
- [Установка](#установка)
- [Настройки](#настройки)
- [Ограничения](#ограничения)
- [Конфиденциальность, лицензия и права](#конфиденциальность)

### Почему Drova Bot

#### 1. Вы узнаёте о свободной станции первым

Популярная станция бывает свободна минуту-две. Бот проверяет каждую станцию с подписчиками раз в 30 секунд и сразу присылает сообщение. Если станцию займут в течение 5 минут, сообщение само исправится на «уже заняли, играют в X», и вы не откроете снова занятую станцию.

#### 2. Статусы совпадают с drova.io

Состояния станций читаются так же, как их читает сайт: «готов», «занят» (в том числе пока клиент подключается) и «недоступен». У занятой станции показывается игра, в которую играют сейчас, после сессии она становится игрой последней сессии. Правила сверены с кодом сайта и тремя часами наблюдения за настоящими станциями.

#### 3. Всё о станции в одной карточке

В карточке видно статус и сколько он длится, текущую или последнюю игру и длительность сессии, город, число игр, железо (процессор, видеокарты, память), описание с drova.io и ваш комментарий. Из карточки открывается история, ссылка для подписки в один клик и удаление станции.

#### 4. Инструменты для мерчантов

Мерчант получает сообщение, когда станция свободна, но ещё скрыта от игроков, может включить для каждой станции уведомления «сессия началась» и «ушла в офлайн» и видит статистику за 7 дней: загрузку, сессии, простой, пиковые часы и топ игр.

#### 5. Рассчитан на сервер

Один статический бинарник без OpenSSL, SQLite с автоматическими миграциями по версиям и резервной копией перед каждой миграцией, watchdog systemd, постоянная очередь сообщений с учётом лимитов Telegram и оповещения админа при сбоях API Drova.

#### 6. Открытый код и минимум данных

Исходный код открыт под лицензией GPL-3.0-or-later. Бот хранит только ваш Telegram ID, подписки, комментарии и настройки, а `/forget` удаляет всё сразу.

### Что умеет бот

- **Уведомления:** станция освободилась или вернулась из офлайна, с игрой последней сессии и её длительностью; сообщение само обновится, если станцию займут в течение 5 минут.
- **Статус и список:** `/status` и `/list` показывают все подписки со значком, числом ожидающих и текущей игрой у занятых станций. Данные не старше одного цикла опроса.
- **Карточка станции:** история, комментарий, ссылка «поделиться», удаление, ссылка на drova.io, железо и описание.
- **История:** смены статуса за 24 часа с играми для всех; статистика за 7 дней для мерчантов и админа.
- **Удалённые станции:** если Drova час отвечает «не найдена», бот спросит, удалить ли станцию. Удалённая станция проверяется раз в час. Если она вернётся, придёт сообщение, если нет, подписка удалится через 30 дней.
- **Тишина:** на 3, 8, 24 часа или до отмены, а также тихие часы в вашем часовом поясе, когда сообщения приходят без звука.
- **Подписки:** до 5 у игроков и до 20 у мерчантов по умолчанию, экспорт и импорт с комментариями, ссылки в один клик `t.me/drova_servers_bot?start=<uuid станции>`, которые мерчант может добавить в описание станции.
- **Для админа:** статистика, последние ошибки, рассылка с подтверждением, оповещение, когда API Drova недоступно или присылает неизвестное состояние станции.

### Версия 2 в сравнении с Python-ботом

Версия 1 была написана на Python. Её код лежит в ветке [`python`](https://github.com/Mahiron-hq/drova-bot/tree/python) и больше не обновляется.

| | 1.0 (Python) | 2.0 (Rust) |
|---|---|---|
| Интервал опроса | 60 секунд | 30 секунд |
| Игра сессии | Нет | Текущая игра в списках, последняя в уведомлениях и карточке |
| Карточка станции | Нет | Статус, игра, железо, описание, история, «поделиться» |
| Сообщение, когда станцию снова заняли | Пометка «Уже заняли» | Та же пометка с игрой, в которую играют |
| Уведомления мерчанта | Свободная скрытая станция | Ещё «сессия началась» и «ушла в офлайн» по каждой станции |
| Статистика мерчанта за 7 дней | Средняя сессия и простой | Ещё загрузка, число сессий, пиковые часы и топ игр |
| Тихие часы и часовой пояс | Нет | Есть |
| Удалённые станции | Проверяются бесконечно | Вопрос подписчикам, проверка раз в час, удаление через 30 дней |
| Доставка сообщений | Прямая отправка | Постоянная очередь с лимитами Telegram и повторами |
| База данных | Исходная схема | Миграции по версиям, старая база переносится без потерь |
| Запуск на сервере | Python с зависимостями | Один статический бинарник, watchdog systemd |

### Как пользоваться

1. Откройте [@drova_servers_bot](https://t.me/drova_servers_bot) и нажмите «Запустить».
2. Выберите роль: пользователь или мерчант. Сменить её можно позже через `/role`.
3. Пришлите ссылку на станцию вида `https://drova.io/stations/<uuid>`. Можно прислать несколько ссылок одним сообщением.
4. Ждите сообщения. `/status` показывает все станции сразу, `/list` открывает карточки.
5. Настройки, тишина и тихие часы в `/settings`, полная справка в `/help`.

### Установка

Нужна, только если вы хотите запустить свою копию бота. Требования: Linux x86_64 с systemd и токен бота от [@BotFather](https://t.me/BotFather).

1. Скачайте `drova-bot-<версия>-linux-x86_64.tar.gz` и `SHA256SUMS.txt` из [релизов](https://github.com/Mahiron-hq/drova-bot/releases/latest) и проверьте хеш: `sha256sum -c SHA256SUMS.txt --ignore-missing`.
2. Распакуйте и установите бинарник: `sudo install -m 755 drova-bot /usr/local/bin/drova-bot`.
3. Создайте пользователя и папку данных: `sudo useradd --system --home /var/lib/drova-bot drova-bot && sudo install -d -o drova-bot -m 750 /var/lib/drova-bot`.
4. Скопируйте [`deploy/drova-bot.env.example`](deploy/drova-bot.env.example) в `/etc/drova-bot/drova-bot.env`, укажите `BOT_TOKEN` и `ADMIN_IDS` и оставьте файл доступным только root (`chmod 600`).
5. Установите [`deploy/drova-bot.service`](deploy/drova-bot.service) в `/etc/systemd/system/` и выполните `sudo systemctl enable --now drova-bot`.

Логи: `journalctl -u drova-bot -f`. Бэкапы: [`deploy/drova-backup`](deploy/drova-backup) с таймером каждый день делает целостную копию базы.

#### Переход с Python-бота

Старую базу нужно один раз конвертировать, пока старый бот остановлен:

```sh
drova-bot migrate-legacy drova_monitor.old.sqlite3 drova_monitor.sqlite3
```

Старый файл не меняется. Переносятся пользователи, роли, тишина, подписки, комментарии, история статусов за последние 90 дней и состояние еженедельной рассылки. Представления `monitoring`, `user_settings` и `server_comments` сохраняют старые имена колонок для внешних инструментов.

#### Сборка из исходников

```sh
cargo build --release
cargo test
```

Бинарники релизов собирает GitHub Actions как статические исполняемые файлы `x86_64-unknown-linux-musl`.

### Настройки

Все настройки задаются переменными окружения (или файлом `.env` в рабочей папке).

| Переменная | По умолчанию | Значение |
|---|---|---|
| `BOT_TOKEN` | обязательно | Токен бота Telegram |
| `DATABASE_PATH` | `drova_monitor.sqlite3` | Файл базы SQLite |
| `ADMIN_IDS` | пусто | Telegram ID админов через запятую |
| `POLL_INTERVAL_SECONDS` | `30` | Пауза между циклами опроса |
| `POLL_CONCURRENCY` | `5` | Параллельные запросы к API Drova |
| `REQUEST_DELAY_SECONDS` | `0.7` | Пауза после каждого запроса |
| `HTTP_TIMEOUT_SECONDS` | `15` | Таймаут API Drova |
| `MAX_SUBSCRIPTIONS_USER` | `5` | Лимит подписок для пользователей |
| `MAX_SUBSCRIPTIONS_MERCHANT` | `20` | Лимит подписок для мерчантов |
| `DONATE_TEXT_FILE` | пусто | Файл Telegram HTML для `/donate` |
| `WEEKLY_REMINDER_FILE` | пусто | Файл Telegram HTML, который отправляется по понедельникам в 19:00 по Москве |
| `ALLOWED_CHAT_IDS` | пусто | Тестовый режим: бот ничего не отправляет вне этих чатов |
| `LOG_LEVEL` | `INFO` | `ERROR`, `WARN`, `INFO`, `DEBUG` |

### Ограничения

- Бот использует только публичное API Drova. История сессий, доходы и другие данные аккаунта мерчанта ему недоступны.
- Смена статуса доходит до вас за один цикл опроса (по умолчанию 30 секунд) плюс время доставки Telegram.
- Пока клиент подключается (до 2-3 минут), станция уже показывается занятой, даже если подключение потом сорвётся.
- Интерфейс бота только на русском языке.

### Конфиденциальность

Бот хранит только Telegram ID, подписки, комментарии и настройки, отправленные сообщения держит 30 дней, историю станций 90 дней. Подробно: [PRIVACY.md](PRIVACY.md).

### Отказ от ответственности

Бот предоставляется «как есть», без гарантий. Сообщение может прийти с опозданием или не прийти, если Drova или Telegram недоступны.

Drova Bot – независимый проект с открытым кодом. Он не связан с Drova и не одобрен ею. Название Drova упоминается только для обозначения того, с чем работает бот.

### Лицензия

GPL-3.0-or-later, см. [LICENSE](LICENSE). Сообщить об уязвимости: [SECURITY.md](SECURITY.md). История изменений: [CHANGELOG.md](CHANGELOG.md). Версия на Python: ветка [`python`](https://github.com/Mahiron-hq/drova-bot/tree/python).

Если бот вам полезен, поставьте звезду репозиторию на GitHub, это помогает проекту.

---

<div align="center">

Copyright © 2026 Lev Burmistrov (Mahiron-hq)
<br>
Mahiron.HQ@protonmail.com | t.me/mahiron_hq

</div>
