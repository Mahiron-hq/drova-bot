<div align="center">

# Drova Bot 1.0 (Python)

**The first version of the Telegram bot that watches Drova stations. Not maintained.**
<br>
**Первая версия Telegram-бота, который следит за станциями Drova. Не поддерживается.**

[![License](https://img.shields.io/badge/license-CC%20BY--NC--SA%204.0-blue?style=flat-square)](LICENSE)
[![Python](https://img.shields.io/badge/python-3-555?style=flat-square)](https://www.python.org)

### [Current version / Текущая версия](https://github.com/Mahiron-hq/drova-bot)

**[English](#english)** · **[Русский](#русский)**

</div>

---

## English

This branch keeps the code of version 1.0 of [@drova_servers_bot](https://t.me/drova_servers_bot), written in Python with aiogram. Since version 2.0 the bot is written in Rust; the code, releases and documentation are in the [`main`](https://github.com/Mahiron-hq/drova-bot) branch.

This version gets no updates or fixes. It is kept for history and for those who want to see how the bot worked before.

### Running

```sh
python -m venv venv
venv/bin/pip install -r requirements.txt
BOT_TOKEN=<token> ADMIN_IDS=<your id> venv/bin/python bot.py
```

Main settings: `BOT_TOKEN`, `DATABASE_PATH` (default `drova_monitor.sqlite3`), `ADMIN_IDS`, `POLL_INTERVAL_SECONDS` (default 60). The database of this version can be converted for version 2 with `drova-bot migrate-legacy`.

### License

[CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/) (Attribution-NonCommercial-ShareAlike 4.0 International), see [LICENSE](LICENSE).

---

## Русский

В этой ветке лежит код версии 1.0 бота [@drova_servers_bot](https://t.me/drova_servers_bot), написанной на Python с aiogram. Начиная с версии 2.0 бот написан на Rust, код, релизы и документация находятся в ветке [`main`](https://github.com/Mahiron-hq/drova-bot).

Эта версия больше не обновляется и не исправляется. Она сохранена для истории и для тех, кому интересно, как бот работал раньше.

### Запуск

```sh
python -m venv venv
venv/bin/pip install -r requirements.txt
BOT_TOKEN=<токен> ADMIN_IDS=<ваш id> venv/bin/python bot.py
```

Основные настройки: `BOT_TOKEN`, `DATABASE_PATH` (по умолчанию `drova_monitor.sqlite3`), `ADMIN_IDS`, `POLL_INTERVAL_SECONDS` (по умолчанию 60). Базу этой версии можно перенести в версию 2 командой `drova-bot migrate-legacy`.

### Лицензия

[CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/) (Attribution-NonCommercial-ShareAlike 4.0 International), см. [LICENSE](LICENSE).

---

<div align="center">

Copyright © 2026 Lev Burmistrov (Mahiron-hq)
<br>
Mahiron.HQ@protonmail.com | t.me/mahiron_hq

</div>
