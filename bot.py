from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import re
import sys
import time
from collections import OrderedDict
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any, Literal
from urllib.parse import urlparse

import aiosqlite
import httpx
from aiogram import Bot, Dispatcher, F, Router
from aiogram.exceptions import TelegramAPIError, TelegramForbiddenError, TelegramNetworkError
from aiogram.filters import Command, CommandStart
from aiogram.fsm.context import FSMContext
from aiogram.fsm.state import State, StatesGroup
from aiogram.types import (
    BotCommand,
    BotCommandScopeChat,
    BufferedInputFile,
    CallbackQuery,
    InlineKeyboardButton,
    InlineKeyboardMarkup,
    KeyboardButton,
    Message,
    ReplyKeyboardMarkup,
)
from apscheduler.schedulers.asyncio import AsyncIOScheduler
from dotenv import load_dotenv

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

# "offline" — сервер недоступен (не занят и не готов)
Status = Literal["ready", "busy", "offline"]
LOG = logging.getLogger("drova_bot")
MAX_SUBSCRIPTIONS_PER_USER = 5
COMMENT_MAX_LEN = 500  # максимальная длина комментария к серверу (дублируется в set_comment)
MOSCOW_TZ = timezone(timedelta(hours=3))
DONATE_CAMPAIGN_KIND = "weekly_donate"
ADMIN_ERROR_KIND = "admin_error"
# A single failed poll is usually a transient timeout; only a streak is worth an admin message.
ADMIN_ALERT_FAIL_STREAK = 3
ADMIN_ALERT_MIN_INTERVAL_SECONDS = 30 * 60
# Undelivered admin alerts older than this are dropped instead of being flushed in bulk later.
ADMIN_ERROR_MAX_AGE_MINUTES = 60

# Сколько секунд после уведомления об освобождении бот продолжает следить за сервером,
# чтобы поймать повторное занятие и отправить апдейт «Уже заняли».
READY_WATCH_SECONDS = 300  # 5 минут
HISTORY_PUBLIC_HOURS = 24
HISTORY_STATS_DAYS = 7
HISTORY_CALLBACK_PREFIX = "history:"
STATUS_HISTORY_PER_URL_LIMIT = 1_000
STATUS_HISTORY_ERROR_LIMIT = 1_000

# Минимальный интервал между нажатиями кнопки «Обновить» одним пользователем.
REFRESH_COOLDOWN_SECONDS = 5.0
# Словарь анти-спама: user_id -> monotonic-время последнего нажатия «Обновить».
_refresh_cooldown: dict[int, float] = {}
# При превышении этого числа записей вычищаем протухшие (старше REFRESH_COOLDOWN_SECONDS).
_REFRESH_COOLDOWN_MAX_SIZE = 10_000


def _check_refresh_cooldown(user_id: int) -> float | None:
    """Проверить кулдаун кнопки «Обновить» для пользователя.

    Возвращает оставшееся время ожидания (секунды) если кулдаун активен,
    либо None — если действие разрешено. При разрешении сразу обновляет метку.
    При превышении _REFRESH_COOLDOWN_MAX_SIZE вычищает протухшие записи.
    """
    now = time.monotonic()
    last = _refresh_cooldown.get(user_id, 0.0)
    remaining = REFRESH_COOLDOWN_SECONDS - (now - last)
    if remaining > 0:
        return remaining
    # Разрешаем — обновляем метку и при необходимости вычищаем кэш
    if len(_refresh_cooldown) >= _REFRESH_COOLDOWN_MAX_SIZE:
        cutoff = now - REFRESH_COOLDOWN_SECONDS
        stale = [uid for uid, ts in _refresh_cooldown.items() if ts < cutoff]
        for uid in stale:
            del _refresh_cooldown[uid]
        LOG.debug("Refresh cooldown cache: evicted %d stale entries", len(stale))
    _refresh_cooldown[user_id] = now
    return None

# Текст команды /donate (используется в нескольких местах)
DONATE_TEXT = (
    "💙 Поддержать проект\n\n"
    "Стабильная работа бота требует времени и ресурсов\\. Если вам нравится сервис, "
    "вы можете отправить чаевые автору — любая поддержка вдохновляет на новые функции\\!\n\n"
    "Картой или СБП \\(через CloudTips\\): https://pay\\.cloudtips\\.ru/p/7a82795b"
)

WEEKLY_DONATE_TEXT = (
    "👋 Надеюсь, бот помогает вам не упускать свободные серверы\\!\n\n"
    "Если он оказался полезным, вы можете поддержать проект — "
    "это помогает развивать его и добавлять новые функции\\. "
    "Подробности: /donate\n\n"
)

# Rate limiting: минимальный интервал между повторными подписками на один URL
SUBSCRIBE_RATE_LIMIT_SECONDS = 30.0
# Максимальный размер rate-cache; при превышении чистим устаревшие записи
_RATE_CACHE_MAX_SIZE = 10_000
# asyncio однопоточен, но Lock добавлен явно — для наглядности и защиты от будущих правок
_subscribe_rate_lock = asyncio.Lock()
_subscribe_rate_cache: dict[tuple[int, str], float] = {}  # (user_id, url) -> monotonic timestamp


def _evict_rate_cache(now: float) -> None:
    """Вычистить протухшие записи из _subscribe_rate_cache, если он вырос сверх лимита.

    Вызывается синхронно внутри блока `async with _subscribe_rate_lock`,
    поэтому изменение словаря безопасно без дополнительных блокировок.
    """
    if len(_subscribe_rate_cache) > _RATE_CACHE_MAX_SIZE:
        stale = [
            k for k, ts in _subscribe_rate_cache.items()
            if now - ts > SUBSCRIBE_RATE_LIMIT_SECONDS
        ]
        for k in stale:
            del _subscribe_rate_cache[k]
        LOG.debug("Rate cache: evicted %d stale entries, size=%d", len(stale), len(_subscribe_rate_cache))

# Словарь блокировок для команды /status: не даём одному пользователю
# запустить два параллельных обновления (гонка записи в историю).
# OrderedDict + ограничение размера (_USER_LOCKS_MAX_SIZE) предотвращают
# утечку памяти при большом числе уникальных пользователей за время работы бота.
_USER_LOCKS_MAX_SIZE = 10_000
_user_status_locks: OrderedDict[int, asyncio.Lock] = OrderedDict()


def get_user_status_lock(user_id: int) -> asyncio.Lock:
    """Вернуть (или создать) per-user asyncio.Lock для /status-команды.

    Использует OrderedDict как LRU-кеш: при превышении _USER_LOCKS_MAX_SIZE
    вытесняет самый давно использованный (и незанятый) Lock, чтобы не копить
    по одной записи на каждого уникального пользователя за всё время работы бота.
    """
    # Безопасно без вложенной блокировки: asyncio однопоточен,
    # между созданием Lock и его возвратом нет await.
    if user_id in _user_status_locks:
        _user_status_locks.move_to_end(user_id)
        return _user_status_locks[user_id]
    lock = asyncio.Lock()
    _user_status_locks[user_id] = lock
    # LRU-вытеснение: удаляем самый старый незанятый Lock
    if len(_user_status_locks) > _USER_LOCKS_MAX_SIZE:
        for old_id, old_lock in _user_status_locks.items():
            if not old_lock.locked():
                _user_status_locks.pop(old_id)
                break
    return lock


def release_user_status_lock(user_id: int, lock: asyncio.Lock) -> None:
    """Удалить Lock из словаря, если он больше не нужен.

    Вызывается синхронно сразу после выхода из `async with lock` — в asyncio
    между этими двумя строками не может появиться другая корутина, поэтому
    проверка is/locked() атомарна. Удаляем только свой экземпляр Lock, чтобы
    не затронуть новый, если другая корутина уже успела его пересоздать.
    """
    if _user_status_locks.get(user_id) is lock and not lock.locked():
        _user_status_locks.pop(user_id, None)


URL_RE = re.compile(r"https?://(?:www\.)?drova\.io/stations/[^\s<>()]+", re.IGNORECASE)
STATION_UUID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
    re.IGNORECASE,
)
PUBLIC_SERVER_API_BASE = "https://services.drova.io/server-manager/servers/public"
READY_STATES   = {"READY", "LISTEN", "FREE", "AVAILABLE", "IDLE", "ONLINE", "OPEN"}
BUSY_STATES    = {"BUSY", "HANDSHAKE", "OCCUPIED", "PLAYING", "IN_SESSION", "RESERVED", "TAKEN"}
OFFLINE_STATES = {"OFFLINE", "DISCONNECTED", "UNAVAILABLE", "MAINTENANCE", "DOWN", "INACTIVE"}
HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
        "AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/124.0.0.0 Safari/537.36"
    ),
    "Accept": "application/json,text/plain,*/*",
    "Accept-Language": "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7",
    "Cache-Control": "no-cache",
    "Pragma": "no-cache",
}


class RemoveStates(StatesGroup):
    """FSM-состояния для команды /remove без аргументов."""

    waiting_for_url = State()


class CommentStates(StatesGroup):
    """FSM-состояния для добавления/редактирования комментария к серверу."""

    waiting_for_comment = State()


class ImportStates(StatesGroup):
    """FSM-состояния для команды /import."""

    waiting_for_urls = State()


MAIN_KEYBOARD = ReplyKeyboardMarkup(
    keyboard=[
        [KeyboardButton(text="/status"),   KeyboardButton(text="/list")],
        [KeyboardButton(text="/comments"), KeyboardButton(text="/delete")],
        [KeyboardButton(text="/history"),  KeyboardButton(text="/silence")],
        [KeyboardButton(text="/clear"),    KeyboardButton(text="/help")],
    ],
    resize_keyboard=True,
    input_field_placeholder="Пришлите ссылку Drova или выберите команду",
)

BOT_COMMANDS = [
    BotCommand(command="start",       description="Показать инструкцию"),
    BotCommand(command="list",        description="Мои подписки"),
    BotCommand(command="comments",    description="Управление комментариями к серверам"),
    BotCommand(command="status",      description="Текущий статус серверов"),
    BotCommand(command="history",     description="История и статистика сервера"),
    BotCommand(command="delete",      description="Удалить подписку кнопкой"),
    BotCommand(command="remove",      description="Удалить подписку по ссылке"),
    BotCommand(command="unsubscribe", description="Удалить подписку по номеру из /list"),
    BotCommand(command="clear",       description="Удалить все подписки"),
    BotCommand(command="silence",     description="Включить или выключить тишину"),
    BotCommand(command="role",        description="Сменить роль: пользователь / мерчант"),
    BotCommand(command="export",      description="Экспорт подписок в файл"),
    BotCommand(command="import_subs", description="Импорт подписок из текста"),
    BotCommand(command="donate",      description="Поддержать проект"),
    BotCommand(command="help",        description="Справка"),
]
ADMIN_BOT_COMMANDS = BOT_COMMANDS + [
    BotCommand(command="admin", description="Админ: статистика и ошибки"),
]

CLEAR_CONFIRM_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[
        [
            InlineKeyboardButton(text="Да, удалить", callback_data="clear:yes"),
            InlineKeyboardButton(text="Отмена",      callback_data="clear:no"),
        ]
    ]
)

STATUS_REFRESH_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[[InlineKeyboardButton(text="Обновить", callback_data="status:refresh")]]
)

LIST_REFRESH_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[[InlineKeyboardButton(text="Обновить", callback_data="list:refresh")]]
)

SILENCE_ON_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[
        [
            InlineKeyboardButton(text="На 3 часа",  callback_data="silence:3h"),
            InlineKeyboardButton(text="На 8 часов", callback_data="silence:8h"),
        ],
        [
            InlineKeyboardButton(text="На 24 часа", callback_data="silence:24h"),
            InlineKeyboardButton(text="До отмены",  callback_data="silence:forever"),
        ],
    ]
)

SILENCE_OFF_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[[
        InlineKeyboardButton(text="🔔 Включить уведомления", callback_data="silence:off")
    ]]
)

ROLE_SELECT_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[
        [
            InlineKeyboardButton(text="👤 Я пользователь", callback_data="role:user"),
            InlineKeyboardButton(text="🏪 Я мерчант",      callback_data="role:merchant"),
        ]
    ]
)

ROLE_CHANGE_TO_MERCHANT_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[[
        InlineKeyboardButton(text="🏪 Сменить на мерчанта", callback_data="role:merchant")
    ]]
)

ROLE_CHANGE_TO_USER_KEYBOARD = InlineKeyboardMarkup(
    inline_keyboard=[[
        InlineKeyboardButton(text="👤 Сменить на пользователя", callback_data="role:user")
    ]]
)


def comment_prompt_keyboard(subscription_id: int) -> InlineKeyboardMarkup:
    """Клавиатура для предложения добавить комментарий после подписки."""
    return InlineKeyboardMarkup(inline_keyboard=[
        [
            InlineKeyboardButton(text="✏️ Добавить комментарий", callback_data=f"comment:add:{subscription_id}"),
            InlineKeyboardButton(text="Позже",                   callback_data=f"comment:later:{subscription_id}"),
        ]
    ])



# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class Settings:
    bot_token: str
    database_path: str = "drova_monitor.sqlite3"
    poll_interval_seconds: int = 60
    request_delay_seconds: float = 0.7
    http_timeout_seconds: float = 15.0
    cache_ttl_seconds: float = 60.0   # TTL кеша статусов; берётся из CACHE_TTL_SECONDS
    log_level: str = "INFO"
    admin_ids: tuple[int, ...] = ()


@dataclass(frozen=True)
class Snapshot:
    name: str
    status: Status
    raw_status: str
    published: bool = True


@dataclass(frozen=True)
class StatusInterval:
    status: Status
    start: datetime
    end: datetime
    previous_status: Status | None = None
    next_status: Status | None = None


class ParserError(RuntimeError):
    pass


def describe_error(exc: BaseException) -> str:
    """Human-readable error text that is never empty (httpx timeouts stringify to '')."""
    text = str(exc).strip()
    name = type(exc).__name__
    return f"{name}: {text}" if text else name


def parse_admin_ids(value: str) -> tuple[int, ...]:
    ids: list[int] = []
    for item in value.replace(";", ",").split(","):
        item = item.strip()
        if not item:
            continue
        try:
            ids.append(int(item))
        except ValueError:
            print(f"[WARNING] Ignoring invalid ADMIN_IDS item: {item!r}", file=sys.stderr)
    return tuple(ids)


def load_settings() -> Settings:
    load_dotenv()
    token = os.getenv("BOT_TOKEN", "").strip()
    if not token:
        raise RuntimeError("Set BOT_TOKEN in .env.")
    return Settings(
        bot_token=token,
        database_path=os.getenv("DATABASE_PATH", "drova_monitor.sqlite3"),
        poll_interval_seconds=int(os.getenv("POLL_INTERVAL_SECONDS", "60")),
        request_delay_seconds=float(os.getenv("REQUEST_DELAY_SECONDS", "0.7")),
        http_timeout_seconds=float(os.getenv("HTTP_TIMEOUT_SECONDS", "15")),
        cache_ttl_seconds=float(os.getenv("CACHE_TTL_SECONDS", "60")),
        log_level=os.getenv("LOG_LEVEL", "INFO").upper(),
        admin_ids=parse_admin_ids(os.getenv("ADMIN_IDS", "")),
    )


# ---------------------------------------------------------------------------
# URL helpers
# ---------------------------------------------------------------------------

def normalize_url(url: str) -> str | None:
    parsed = urlparse(url.strip().rstrip(".,;!?)\"]'"))
    host = parsed.netloc.lower()
    if host.startswith("www."):
        host = host[4:]
    if parsed.scheme.lower() not in {"http", "https"} or host != "drova.io":
        return None
    if not parsed.path.startswith("/stations/"):
        return None
    station_id = parsed.path.rstrip("/").split("/")[-1]
    if not STATION_UUID_RE.fullmatch(station_id):
        return None
    return f"https://{host}/stations/{station_id.lower()}"


def extract_url(text: str | None) -> str | None:
    if not text:
        return None
    match = URL_RE.search(text)
    return normalize_url(match.group(0)) if match else None


def station_uuid_from_url(url: str) -> str:
    parsed = urlparse(url)
    station_id = parsed.path.rstrip("/").split("/")[-1]
    if not STATION_UUID_RE.fullmatch(station_id):
        raise ParserError(f"Cannot extract Drova station UUID from URL: {url}")
    return station_id.lower()


def public_server_api_url(station_url: str) -> str:
    return f"{PUBLIC_SERVER_API_BASE}/{station_uuid_from_url(station_url)}"


# ---------------------------------------------------------------------------
# Status parsing
# ---------------------------------------------------------------------------

def normalize_status(raw_status: object) -> Status:
    value = str(raw_status).strip().upper()
    if value in READY_STATES:
        return "ready"
    if value in BUSY_STATES:
        return "busy"
    if value in OFFLINE_STATES:
        return "offline"
    # Неизвестный статус — безопаснее считать офлайном, чем падать с ошибкой
    LOG.warning("Unknown station status %r — treating as offline", raw_status)
    return "offline"


def parse_api_snapshot(data: Any) -> Snapshot:
    if not isinstance(data, dict):
        raise ParserError("Drova public API response is not a JSON object")

    name       = data.get("name")
    raw_status = data.get("state")
    uuid       = data.get("uuid")

    if not isinstance(uuid, str) or not STATION_UUID_RE.fullmatch(uuid):
        raise ParserError("Drova public API response has no valid uuid")
    if not isinstance(name, str) or not name.strip():
        raise ParserError("Drova public API response has no station name")
    if not raw_status:
        raise ParserError("Drova public API response has no state")

    published = data.get("published", True)
    if not isinstance(published, bool):
        published = True  # безопасный дефолт при нестандартном значении

    return Snapshot(
        name=name.strip(),
        status=normalize_status(raw_status),
        raw_status=str(raw_status),
        published=published,
    )


# ---------------------------------------------------------------------------
# Drova HTTP client with LRU+TTL cache
# ---------------------------------------------------------------------------

class DrovaClient:
    CACHE_MAXSIZE = 1000  # максимальное число URL в LRU-кеше

    def __init__(self, timeout_seconds: float, cache_ttl_seconds: float = 60.0) -> None:
        self.client = httpx.AsyncClient(headers=HEADERS, timeout=timeout_seconds, follow_redirects=True)
        # TTL берётся из настроек (CACHE_TTL_SECONDS), по умолчанию 60 с.
        # Монитор опрашивает серверы каждые POLL_INTERVAL_SECONDS секунд и пишет свежий
        # снимок в кеш (use_cache=False). Все пользовательские запросы (/status, /list)
        # отвечают из кеша и не нагружают Drova API лишними запросами.
        # После истечения TTL кеш для конкретного URL сбрасывается и при следующем
        # обращении делается живой запрос.
        self._cache_ttl = cache_ttl_seconds
        # OrderedDict используется как LRU-кеш с ограниченным размером.
        # Ключ — URL, значение — (monotonic_timestamp, Snapshot).
        self._cache: OrderedDict[str, tuple[float, Snapshot]] = OrderedDict()

    async def snapshot(self, url: str, use_cache: bool = True) -> Snapshot:
        """Получить статус станции.

        use_cache=True  — вернуть кешированный результат, если он не старше _cache_ttl.
                          Устаревший ключ явно удаляется, чтобы не занимать память.
        use_cache=False — всегда делать живой запрос; результат обновляет кеш
                          (используется монитором для периодического опроса).
        """
        if use_cache:
            entry = self._cache.get(url)
            if entry is not None:
                ts, snap = entry
                if time.monotonic() - ts < self._cache_ttl:
                    self._cache.move_to_end(url)
                    LOG.debug("Cache hit for %s", url)
                    return snap
                # TTL истёк — явно удаляем устаревшую запись
                del self._cache[url]
                LOG.debug("Cache expired and removed for %s", url)

        response = await self.client.get(public_server_api_url(url))
        response.raise_for_status()
        snap = parse_api_snapshot(response.json())

        # Сохраняем в кеш (или обновляем метку времени для use_cache=False)
        self._cache[url] = (time.monotonic(), snap)
        self._cache.move_to_end(url)
        # LRU-вытеснение при переполнении
        if len(self._cache) > self.CACHE_MAXSIZE:
            evicted_url, _ = self._cache.popitem(last=False)
            LOG.debug("Cache evicted LRU entry: %s", evicted_url)

        return snap

    async def close(self) -> None:
        await self.client.aclose()


# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------

class Database:
    def __init__(self, path: str) -> None:
        self.path = path
        self.db: aiosqlite.Connection | None = None

    async def connect(self) -> None:
        self.db = await aiosqlite.connect(self.path)
        self.db.row_factory = aiosqlite.Row
        await self.db.execute("PRAGMA journal_mode=WAL")

    async def close(self) -> None:
        if self.db:
            await self.db.close()
            self.db = None

    async def init(self) -> None:
        db = self.connection
        # Сначала выполняем миграцию (для уже существующих БД),
        # затем CREATE TABLE IF NOT EXISTS для новых.
        await self._migrate_add_offline_status()
        await self._migrate_add_mute_until()
        await self._migrate_add_is_merchant()
        await self._migrate_add_role_selected()
        await db.executescript(
            """
            CREATE TABLE IF NOT EXISTS monitoring (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id BIGINT NOT NULL,
                url TEXT NOT NULL,
                server_name TEXT NOT NULL,
                last_status VARCHAR(10) NOT NULL
                    CHECK(last_status IN ('ready', 'busy', 'offline')),
                pending_notify INTEGER NOT NULL DEFAULT 1,
                UNIQUE(user_id, url)
            );
            CREATE TABLE IF NOT EXISTS user_settings (
                user_id BIGINT PRIMARY KEY,
                muted INTEGER NOT NULL DEFAULT 0,
                mute_until TEXT DEFAULT NULL,
                is_merchant INTEGER NOT NULL DEFAULT 0,
                role_selected INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS status_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL,
                server_name TEXT,
                status VARCHAR(10),
                raw_status TEXT,
                error TEXT,
                checked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS status_transitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL,
                server_name TEXT NOT NULL,
                from_status VARCHAR(10)
                    CHECK(from_status IS NULL OR from_status IN ('ready', 'busy', 'offline')),
                to_status VARCHAR(10) NOT NULL
                    CHECK(to_status IN ('ready', 'busy', 'offline')),
                changed_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS server_comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id BIGINT NOT NULL,
                url TEXT NOT NULL,
                comment TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(user_id, url)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS ux_monitoring_user_url
                ON monitoring(user_id, url);
            CREATE INDEX IF NOT EXISTS ix_monitoring_url
                ON monitoring(url);
            CREATE INDEX IF NOT EXISTS ix_status_history_url_time
                ON status_history(url, checked_at);
            CREATE INDEX IF NOT EXISTS ix_status_history_error_time
                ON status_history(error, checked_at);
            CREATE INDEX IF NOT EXISTS ix_status_transitions_url_time
                ON status_transitions(url, changed_at);
            CREATE INDEX IF NOT EXISTS ix_comments_user
                ON server_comments(user_id);
            CREATE TABLE IF NOT EXISTS ready_watch (
                url TEXT PRIMARY KEY,
                expires_at TEXT NOT NULL,
                messages TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS notification_campaigns (
                kind TEXT NOT NULL,
                due_at TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (kind, due_at)
            );
            CREATE TABLE IF NOT EXISTS notification_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                user_id BIGINT NOT NULL,
                text TEXT NOT NULL,
                parse_mode TEXT,
                bypass_mute INTEGER NOT NULL DEFAULT 0,
                not_before TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                sent_at TEXT
            );
            CREATE INDEX IF NOT EXISTS ix_notification_queue_due
                ON notification_queue(sent_at, not_before, id);
            CREATE INDEX IF NOT EXISTS ix_notification_queue_user_kind
                ON notification_queue(user_id, kind, created_at);
            CREATE UNIQUE INDEX IF NOT EXISTS ux_notification_queue_delivery
                ON notification_queue(kind, user_id, not_before);
            """
        )
        await db.execute("DROP INDEX IF EXISTS ux_monitoring_user")
        columns = [row["name"] for row in await db.execute_fetchall("PRAGMA table_info(monitoring)")]
        if "pending_notify" not in columns:
            await db.execute(
                "ALTER TABLE monitoring ADD COLUMN pending_notify INTEGER NOT NULL DEFAULT 1"
            )
            await db.execute("UPDATE monitoring SET pending_notify = 1")
        await self._seed_missing_status_transitions()
        await db.commit()

    async def _migrate_add_offline_status(self) -> None:
        """Пересоздать таблицу monitoring, если в CHECK-ограничении нет 'offline'.

        SQLite не поддерживает ALTER TABLE ADD/MODIFY CONSTRAINT,
        поэтому единственный способ обновить ограничение — пересоздать таблицу.
        """
        db = self.connection
        rows = await db.execute_fetchall(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='monitoring'"
        )
        if not rows:
            return  # Таблица ещё не существует — будет создана с правильной схемой
        existing_sql: str = rows[0]["sql"] or ""
        if "'offline'" in existing_sql or '"offline"' in existing_sql:
            return  # Уже мигрировано
        LOG.info("Migrating 'monitoring' table to add 'offline' status support…")
        # executescript() неявно делает COMMIT перед запуском, транзакция не нужна
        await db.executescript("""
            ALTER TABLE monitoring RENAME TO _monitoring_pre_offline;
            CREATE TABLE monitoring (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id BIGINT NOT NULL,
                url TEXT NOT NULL,
                server_name TEXT NOT NULL,
                last_status VARCHAR(10) NOT NULL
                    CHECK(last_status IN ('ready', 'busy', 'offline')),
                pending_notify INTEGER NOT NULL DEFAULT 1,
                UNIQUE(user_id, url)
            );
            INSERT INTO monitoring SELECT * FROM _monitoring_pre_offline;
            DROP TABLE _monitoring_pre_offline;
            CREATE UNIQUE INDEX IF NOT EXISTS ux_monitoring_user_url
                ON monitoring(user_id, url);
            CREATE INDEX IF NOT EXISTS ix_monitoring_url
                ON monitoring(url);
        """)
        await db.commit()
        LOG.info("Migration to 'offline' status complete.")

    async def _migrate_add_mute_until(self) -> None:
        """Добавить колонку mute_until в user_settings, если её нет."""
        db = self.connection
        columns = [row["name"] for row in await db.execute_fetchall("PRAGMA table_info(user_settings)")]
        if "mute_until" not in columns and columns:  # таблица уже существует
            await db.execute("ALTER TABLE user_settings ADD COLUMN mute_until TEXT DEFAULT NULL")
            await db.commit()
            LOG.info("Migration: added mute_until column to user_settings")

    async def _migrate_add_is_merchant(self) -> None:
        """Добавить колонку is_merchant в user_settings, если её нет."""
        db = self.connection
        columns = [row["name"] for row in await db.execute_fetchall("PRAGMA table_info(user_settings)")]
        if columns and "is_merchant" not in columns:
            await db.execute(
                "ALTER TABLE user_settings ADD COLUMN is_merchant INTEGER NOT NULL DEFAULT 0"
            )
            await db.commit()
            LOG.info("Migration: added is_merchant column to user_settings")

    async def _migrate_add_role_selected(self) -> None:
        """Добавить колонку role_selected в user_settings, если её нет.

        role_selected=1 означает, что пользователь явно выбрал роль хотя бы раз.
        Используется в /start, чтобы не показывать повторный выбор роли.
        """
        db = self.connection
        columns = [row["name"] for row in await db.execute_fetchall("PRAGMA table_info(user_settings)")]
        if columns and "role_selected" not in columns:
            await db.execute(
                "ALTER TABLE user_settings ADD COLUMN role_selected INTEGER NOT NULL DEFAULT 0"
            )
            await db.commit()
            LOG.info("Migration: added role_selected column to user_settings")

    async def record_status(self, url: str, snapshot: Snapshot) -> None:
        await self.connection.execute(
            """
            INSERT INTO status_history (url, server_name, status, raw_status, error)
            VALUES (?, ?, ?, ?, NULL)
            """,
            (url, snapshot.name, snapshot.status, snapshot.raw_status),
        )
        await self._record_status_transition(url, snapshot.name, snapshot.status)
        await self._trim_history()
        await self.connection.commit()  # единственный commit для INSERT + trim

    async def _record_status_transition(self, url: str, name: str, status: Status) -> None:
        """Записать только реальный переход статуса для пользовательской истории.

        status_history хранит технический журнал опросов, а status_transitions —
        компактную бизнес-историю: первая известная точка и дальнейшие смены.
        Метод намеренно не делает commit; он вызывается внутри record_status().
        """
        rows = await self.connection.execute_fetchall(
            """
            SELECT to_status
            FROM status_transitions
            WHERE url = ?
            ORDER BY datetime(changed_at) DESC, id DESC
            LIMIT 1
            """,
            (url,),
        )
        previous = rows[0]["to_status"] if rows else None
        if previous == status:
            return
        await self.connection.execute(
            """
            INSERT INTO status_transitions (url, server_name, from_status, to_status, changed_at)
            VALUES (?, ?, ?, ?, ?)
            """,
            (url, name, previous, status, datetime.now(timezone.utc).isoformat()),
        )

    async def _seed_missing_status_transitions(self) -> None:
        """Создать начальную точку истории для URL, у которых её ещё нет."""
        await self.connection.execute(
            """
            INSERT INTO status_transitions (url, server_name, from_status, to_status, changed_at)
            SELECT
                m.url,
                m.server_name,
                NULL,
                COALESCE(
                    (
                        SELECT sh.status
                        FROM status_history AS sh
                        WHERE sh.url = m.url
                          AND sh.error IS NULL
                          AND sh.status IN ('ready', 'busy', 'offline')
                        ORDER BY datetime(sh.checked_at) DESC, sh.id DESC
                        LIMIT 1
                    ),
                    m.last_status
                ) AS to_status,
                ?
            FROM monitoring AS m
            WHERE NOT EXISTS (
                SELECT 1 FROM status_transitions AS st WHERE st.url = m.url
            )
              AND m.id = (
                  SELECT MIN(m2.id)
                  FROM monitoring AS m2
                  WHERE m2.url = m.url
              )
            """,
            (datetime.now(timezone.utc).isoformat(),),
        )

    async def record_error(self, url: str, error: str) -> None:
        error = error.strip() or "(нет описания ошибки)"
        await self.connection.execute(
            """
            INSERT INTO status_history (url, server_name, status, raw_status, error)
            VALUES (?, NULL, NULL, NULL, ?)
            """,
            (url, error[:1000]),
        )
        await self._trim_history()
        await self.connection.commit()  # единственный commit для INSERT + trim

    async def _trim_history(self) -> None:
        """Удалить старые записи технической истории без глобального перекоса.

        Намеренно НЕ делает commit — вызывающий (record_status / record_error)
        объединяет INSERT и DELETE в одну транзакцию и сам вызывает commit.
        """
        await self.connection.execute(
            """
            DELETE FROM status_history
            WHERE id IN (
                SELECT id
                FROM (
                    SELECT
                        id,
                        ROW_NUMBER() OVER (
                            PARTITION BY url
                            ORDER BY datetime(checked_at) DESC, id DESC
                        ) AS rn
                    FROM status_history
                    WHERE error IS NULL
                )
                WHERE rn > ?
            )
            """,
            (STATUS_HISTORY_PER_URL_LIMIT,),
        )
        await self.connection.execute(
            """
            DELETE FROM status_history
            WHERE id IN (
                SELECT id
                FROM (
                    SELECT
                        id,
                        ROW_NUMBER() OVER (
                            ORDER BY datetime(checked_at) DESC, id DESC
                        ) AS rn
                    FROM status_history
                    WHERE error IS NOT NULL AND error != ''
                )
                WHERE rn > ?
            )
            """,
            (STATUS_HISTORY_ERROR_LIMIT,),
        )

    async def add_subscription(self, user_id: int, url: str, name: str, status: Status) -> str:
        # busy и offline → ждём перехода в ready → pending_notify=1
        pending = 0 if status == "ready" else 1
        existing = await self.connection.execute_fetchall(
            "SELECT id FROM monitoring WHERE user_id = ? AND url = ?",
            (user_id, url),
        )
        if existing:
            await self._record_status_transition(url, name, status)
            await self.connection.execute(
                """
                UPDATE monitoring
                SET server_name = ?, last_status = ?, pending_notify = ?
                WHERE user_id = ? AND url = ?
                """,
                (name, status, pending, user_id, url),
            )
            await self.connection.commit()
            return "updated"

        count = await self.subscription_count(user_id)
        if count >= MAX_SUBSCRIPTIONS_PER_USER:
            return "limit"

        await self.connection.execute(
            """
            INSERT INTO monitoring (user_id, url, server_name, last_status, pending_notify)
            VALUES (?, ?, ?, ?, ?)
            """,
            (user_id, url, name, status, pending),
        )
        await self._record_status_transition(url, name, status)
        await self.connection.commit()
        return "added"

    async def subscription_count(self, user_id: int) -> int:
        rows = await self.connection.execute_fetchall(
            "SELECT COUNT(*) AS count FROM monitoring WHERE user_id = ?",
            (user_id,),
        )
        return int(rows[0]["count"])

    async def urls(self) -> list[str]:
        rows = await self.connection.execute_fetchall(
            "SELECT DISTINCT url FROM monitoring ORDER BY url"
        )
        return [row["url"] for row in rows]

    async def server_by_url(self, url: str) -> aiosqlite.Row | None:
        rows = await self.connection.execute_fetchall(
            """
            SELECT url, server_name
            FROM monitoring
            WHERE url = ?
            ORDER BY id ASC
            LIMIT 1
            """,
            (url,),
        )
        return rows[0] if rows else None

    async def subscriptions_for_url(self, url: str) -> list[aiosqlite.Row]:
        """Вернуть подписчиков, которым может понадобиться уведомление.

        pending_notify=1 выставляется при переходе сервера в 'busy' или 'offline',
        поэтому условия «OR last_status IN (...)» служат страховкой на случай
        рассинхронизации флага.
        """
        return await self.connection.execute_fetchall(
            """
            SELECT
                m.user_id,
                m.url,
                m.server_name,
                m.last_status,
                m.pending_notify,
                CASE
                    WHEN s.muted = 1
                         AND (s.mute_until IS NULL OR datetime(s.mute_until) > datetime('now'))
                    THEN 1 ELSE 0
                END AS muted,
                COALESCE(s.is_merchant, 0) AS is_merchant
            FROM monitoring AS m
            LEFT JOIN user_settings AS s ON s.user_id = m.user_id
            WHERE m.url = ?
              AND (m.pending_notify = 1 OR m.last_status IN ('busy', 'offline'))
            """,
            (url,),
        )

    async def user_muted(self, user_id: int) -> bool:
        rows = await self.connection.execute_fetchall(
            "SELECT muted, mute_until FROM user_settings WHERE user_id = ?",
            (user_id,),
        )
        if not rows or not rows[0]["muted"]:
            return False
        mute_until = rows[0]["mute_until"]
        if mute_until is None:
            return True  # бессрочно
        try:
            until_dt = datetime.fromisoformat(mute_until)
            return datetime.now(timezone.utc) < until_dt
        except ValueError:
            return False

    async def get_mute_info(self, user_id: int) -> tuple[bool, datetime | None]:
        """Вернуть (активна ли тишина, до какого момента UTC или None если бессрочно)."""
        rows = await self.connection.execute_fetchall(
            "SELECT muted, mute_until FROM user_settings WHERE user_id = ?",
            (user_id,),
        )
        if not rows or not rows[0]["muted"]:
            return False, None
        mute_until = rows[0]["mute_until"]
        if mute_until is None:
            return True, None  # бессрочно
        try:
            until_dt = datetime.fromisoformat(mute_until)
            if datetime.now(timezone.utc) < until_dt:
                return True, until_dt
            return False, None  # истекло
        except ValueError:
            return False, None

    async def set_mute(self, user_id: int, until: datetime | None) -> None:
        """Включить тишину. until=None — бессрочно; datetime — до указанного момента."""
        mute_until_str = until.isoformat() if until else None
        await self.connection.execute(
            """
            INSERT INTO user_settings (user_id, muted, mute_until)
            VALUES (?, 1, ?)
            ON CONFLICT(user_id) DO UPDATE SET muted = 1, mute_until = excluded.mute_until
            """,
            (user_id, mute_until_str),
        )
        await self.connection.commit()

    async def unmute(self, user_id: int) -> None:
        """Выключить тишину."""
        await self.connection.execute(
            """
            INSERT INTO user_settings (user_id, muted, mute_until)
            VALUES (?, 0, NULL)
            ON CONFLICT(user_id) DO UPDATE SET muted = 0, mute_until = NULL
            """,
            (user_id,),
        )
        await self.connection.commit()

    async def consume_expired_mutes(self, limit: int = 100) -> list[int]:
        """Выключить истёкшую тишину и вернуть пользователей для статус-отчёта."""
        rows = await self.connection.execute_fetchall(
            """
            SELECT user_id
            FROM user_settings
            WHERE muted = 1
              AND mute_until IS NOT NULL
              AND datetime(mute_until) <= datetime('now')
            ORDER BY datetime(mute_until) ASC
            LIMIT ?
            """,
            (limit,),
        )
        user_ids = [int(row["user_id"]) for row in rows]
        if not user_ids:
            return []
        placeholders = ",".join("?" * len(user_ids))
        await self.connection.execute(
            f"""
            UPDATE user_settings
            SET muted = 0, mute_until = NULL
            WHERE user_id IN ({placeholders})
            """,
            tuple(user_ids),
        )
        await self.connection.commit()
        return user_ids

    async def get_user_role(self, user_id: int) -> bool:
        """Вернуть True если пользователь — мерчант, False если обычный пользователь."""
        rows = await self.connection.execute_fetchall(
            "SELECT is_merchant FROM user_settings WHERE user_id = ?",
            (user_id,),
        )
        if not rows:
            return False  # по умолчанию обычный пользователь
        return bool(rows[0]["is_merchant"])

    async def set_user_role(self, user_id: int, is_merchant: bool) -> None:
        """Установить роль пользователя: True — мерчант, False — обычный пользователь.

        Одновременно выставляет role_selected=1, чтобы /start не предлагал
        повторный выбор роли пользователям, уже сделавшим его осознанно.
        """
        await self.connection.execute(
            """
            INSERT INTO user_settings (user_id, is_merchant, role_selected)
            VALUES (?, ?, 1)
            ON CONFLICT(user_id) DO UPDATE SET
                is_merchant = excluded.is_merchant,
                role_selected = 1
            """,
            (user_id, int(is_merchant)),
        )
        await self.connection.commit()

    async def has_role_selected(self, user_id: int) -> bool:
        """Вернуть True, если пользователь явно выбирал роль хотя бы раз."""
        rows = await self.connection.execute_fetchall(
            "SELECT role_selected FROM user_settings WHERE user_id = ? AND role_selected = 1",
            (user_id,),
        )
        return bool(rows)

    async def mark_busy(self, url: str, name: str) -> None:
        await self.connection.execute(
            "UPDATE monitoring SET server_name = ?, last_status = 'busy', pending_notify = 1 WHERE url = ?",
            (name, url),
        )
        await self.connection.commit()

    async def mark_offline(self, url: str, name: str) -> None:
        """Пометить все подписки на этот URL как offline (сервер недоступен)."""
        await self.connection.execute(
            "UPDATE monitoring SET server_name = ?, last_status = 'offline', pending_notify = 1 WHERE url = ?",
            (name, url),
        )
        await self.connection.commit()

    async def mark_ready(self, url: str, name: str) -> None:
        await self.connection.execute(
            "UPDATE monitoring SET server_name = ?, last_status = 'ready' WHERE url = ?",
            (name, url),
        )
        await self.connection.commit()

    async def mark_ready_for_user_ids(self, url: str, name: str, user_ids: list[int]) -> None:
        """Пометить ready только для конкретных пользователей (используется для мерчантов)."""
        if not user_ids:
            return
        placeholders = ",".join("?" * len(user_ids))
        await self.connection.execute(
            f"UPDATE monitoring SET server_name = ?, last_status = 'ready', pending_notify = 0 "
            f"WHERE url = ? AND user_id IN ({placeholders})",
            (name, url, *user_ids),
        )
        await self.connection.commit()

    async def update_server_name(self, url: str, name: str) -> None:
        """Обновить имя сервера для всех подписчиков URL (вызывается при каждом успешном опросе)."""
        await self.connection.execute(
            "UPDATE monitoring SET server_name = ? WHERE url = ?",
            (name, url),
        )
        await self.connection.commit()

    async def bulk_mark_notified(self, user_ids: list[int], url: str) -> None:
        """Сбросить pending_notify для нескольких пользователей одним запросом."""
        if not user_ids:
            return
        placeholders = ",".join("?" * len(user_ids))
        await self.connection.execute(
            f"UPDATE monitoring SET pending_notify = 0 WHERE url = ? AND user_id IN ({placeholders})",
            (url, *user_ids),
        )
        await self.connection.commit()

    async def subscriptions_for_user(self, user_id: int) -> list[aiosqlite.Row]:
        # Сортируем по id (порядку добавления) — стабильный порядок,
        # который не «съедет» при переименовании сервера в Drova.
        # Это гарантирует, что номер из /list всегда совпадает с
        # номером в /unsubscribe N.
        return await self.connection.execute_fetchall(
            """
            SELECT id, url, server_name, last_status, pending_notify
            FROM monitoring
            WHERE user_id = ?
            ORDER BY id ASC
            """,
            (user_id,),
        )

    async def active_waiting_counts_for_user(
        self, user_id: int, urls: list[str]
    ) -> dict[str, int]:
        """Посчитать других активных пользователей, ожидающих каждый URL.

        Активной считается подписка пользователя без действующего режима тишины.
        Текущий пользователь вычитается из результата, потому что в UI нужен
        ответ на вопрос «сколько ещё людей ждёт освобождения».
        """
        if not urls:
            return {}
        unique_urls = list(dict.fromkeys(urls))
        placeholders = ",".join("?" * len(unique_urls))
        rows = await self.connection.execute_fetchall(
            f"""
            SELECT m.url, COUNT(DISTINCT m.user_id) AS waiters
            FROM monitoring AS m
            LEFT JOIN user_settings AS s ON s.user_id = m.user_id
            WHERE m.url IN ({placeholders})
              AND NOT (
                  COALESCE(s.muted, 0) = 1
                  AND (s.mute_until IS NULL OR datetime(s.mute_until) > datetime('now'))
              )
            GROUP BY m.url
            """,
            tuple(unique_urls),
        )
        counts = {row["url"]: int(row["waiters"]) for row in rows}
        if not await self.user_muted(user_id):
            for url in unique_urls:
                counts[url] = max(0, counts.get(url, 0) - 1)
        return counts

    async def transition_rows(
        self, url: str, since: datetime | None = None
    ) -> list[aiosqlite.Row]:
        if since is None:
            return await self.connection.execute_fetchall(
                """
                SELECT from_status, to_status, changed_at
                FROM status_transitions
                WHERE url = ?
                ORDER BY datetime(changed_at) ASC, id ASC
                """,
                (url,),
            )
        return await self.connection.execute_fetchall(
            """
            SELECT from_status, to_status, changed_at
            FROM status_transitions
            WHERE url = ?
              AND datetime(changed_at) >= datetime(?)
            ORDER BY datetime(changed_at) ASC, id ASC
            """,
            (url, since.isoformat()),
        )

    async def transition_rows_for_window(
        self, url: str, start: datetime, end: datetime
    ) -> list[aiosqlite.Row]:
        """Вернуть переходы внутри окна плюс последнюю точку перед окном."""
        before = await self.connection.execute_fetchall(
            """
            SELECT from_status, to_status, changed_at
            FROM status_transitions
            WHERE url = ?
              AND datetime(changed_at) < datetime(?)
            ORDER BY datetime(changed_at) DESC, id DESC
            LIMIT 1
            """,
            (url, start.isoformat()),
        )
        inside = await self.connection.execute_fetchall(
            """
            SELECT from_status, to_status, changed_at
            FROM status_transitions
            WHERE url = ?
              AND datetime(changed_at) >= datetime(?)
              AND datetime(changed_at) <= datetime(?)
            ORDER BY datetime(changed_at) ASC, id ASC
            """,
            (url, start.isoformat(), end.isoformat()),
        )
        return [*before, *inside]

    async def remove(self, user_id: int, url: str) -> bool:
        cursor = await self.connection.execute(
            "DELETE FROM monitoring WHERE user_id = ? AND url = ?", (user_id, url)
        )
        if cursor.rowcount:
            await self.connection.execute(
                "DELETE FROM server_comments WHERE user_id = ? AND url = ?", (user_id, url)
            )
        await self.connection.commit()
        return cursor.rowcount > 0

    async def remove_by_id(self, user_id: int, subscription_id: int) -> bool:
        rows = await self.connection.execute_fetchall(
            "SELECT url FROM monitoring WHERE user_id = ? AND id = ?",
            (user_id, subscription_id),
        )
        cursor = await self.connection.execute(
            "DELETE FROM monitoring WHERE user_id = ? AND id = ?",
            (user_id, subscription_id),
        )
        if cursor.rowcount and rows:
            await self.connection.execute(
                "DELETE FROM server_comments WHERE user_id = ? AND url = ?",
                (user_id, rows[0]["url"]),
            )
        await self.connection.commit()
        return cursor.rowcount > 0

    async def clear(self, user_id: int) -> int:
        cursor = await self.connection.execute(
            "DELETE FROM monitoring WHERE user_id = ?", (user_id,)
        )
        if cursor.rowcount:
            await self.connection.execute(
                "DELETE FROM server_comments WHERE user_id = ?", (user_id,)
            )
        await self.connection.commit()
        return int(cursor.rowcount)

    async def all_unmuted_user_ids(self) -> list[int]:
        """Вернуть user_id всех пользователей с активными подписками, у которых нет активной тишины."""
        rows = await self.connection.execute_fetchall(
            """
            SELECT DISTINCT m.user_id
            FROM monitoring AS m
            LEFT JOIN user_settings AS s ON s.user_id = m.user_id
            WHERE COALESCE(s.muted, 0) = 0
               OR datetime(s.mute_until) <= datetime('now')
            """
        )
        return [row["user_id"] for row in rows]

    async def campaign_exists(self, kind: str, due_at: datetime) -> bool:
        rows = await self.connection.execute_fetchall(
            "SELECT 1 FROM notification_campaigns WHERE kind = ? AND due_at = ?",
            (kind, due_at.isoformat()),
        )
        return bool(rows)

    async def create_campaign(self, kind: str, due_at: datetime) -> bool:
        cursor = await self.connection.execute(
            "INSERT OR IGNORE INTO notification_campaigns (kind, due_at) VALUES (?, ?)",
            (kind, due_at.isoformat()),
        )
        await self.connection.commit()
        return cursor.rowcount > 0

    async def enqueue_notification(
        self,
        user_id: int,
        kind: str,
        text: str,
        *,
        parse_mode: str | None = None,
        bypass_mute: bool = False,
        not_before: datetime | None = None,
    ) -> None:
        await self.connection.execute(
            """
            INSERT OR IGNORE INTO notification_queue
                (kind, user_id, text, parse_mode, bypass_mute, not_before)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (
                kind,
                user_id,
                text,
                parse_mode,
                1 if bypass_mute else 0,
                (not_before or datetime.now(timezone.utc)).isoformat(),
            ),
        )
        await self.connection.commit()

    async def enqueue_many_notifications(
        self,
        user_ids: list[int],
        kind: str,
        text: str,
        *,
        parse_mode: str | None = None,
        bypass_mute: bool = False,
        not_before: datetime | None = None,
    ) -> int:
        if not user_ids:
            return 0
        due = (not_before or datetime.now(timezone.utc)).isoformat()
        await self.connection.executemany(
            """
            INSERT OR IGNORE INTO notification_queue
                (kind, user_id, text, parse_mode, bypass_mute, not_before)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            [
                (kind, user_id, text, parse_mode, 1 if bypass_mute else 0, due)
                for user_id in user_ids
            ],
        )
        await self.connection.commit()
        return len(user_ids)

    async def due_notifications(self, limit: int = 50) -> list[aiosqlite.Row]:
        return await self.connection.execute_fetchall(
            """
            SELECT id, kind, user_id, text, parse_mode, attempts
            FROM notification_queue
            WHERE sent_at IS NULL
              AND datetime(not_before) <= datetime('now')
            ORDER BY id ASC
            LIMIT ?
            """,
            (limit,),
        )

    async def mark_notification_sent(self, notification_id: int) -> None:
        await self.connection.execute(
            "UPDATE notification_queue SET sent_at = CURRENT_TIMESTAMP WHERE id = ?",
            (notification_id,),
        )
        await self.connection.commit()

    async def mark_notification_failed(self, notification_id: int, error: str) -> None:
        await self.connection.execute(
            """
            UPDATE notification_queue
            SET attempts = attempts + 1,
                last_error = ?,
                -- the id suffix keeps (kind, user_id, not_before) unique when several rows fail in the same second
                not_before = datetime('now', '+' || MIN(attempts + 1, 60) || ' minutes')
                    || '.' || printf('%06d', id % 1000000)
            WHERE id = ?
            """,
            (error[:1000], notification_id),
        )
        await self.connection.commit()

    async def drop_stale_notifications(self, kind: str, max_age_minutes: int) -> int:
        """Mark undelivered notifications of ``kind`` older than ``max_age_minutes`` as dropped.

        Returns the number of dropped rows.
        """
        cursor = await self.connection.execute(
            """
            UPDATE notification_queue
            SET sent_at = CURRENT_TIMESTAMP,
                last_error = 'dropped: stale'
            WHERE kind = ?
              AND sent_at IS NULL
              AND datetime(created_at) <= datetime('now', '-' || ? || ' minutes')
            """,
            (kind, max_age_minutes),
        )
        await self.connection.commit()
        return cursor.rowcount

    async def admin_stats(self) -> dict[str, int]:
        rows = await self.connection.execute_fetchall(
            """
            SELECT
                (SELECT COUNT(DISTINCT user_id) FROM monitoring)       AS users,
                (SELECT COUNT(*) FROM monitoring)                       AS subscriptions,
                (SELECT COUNT(DISTINCT url) FROM monitoring)            AS unique_urls,
                (SELECT COUNT(*) FROM user_settings
                 WHERE muted = 1
                   AND (mute_until IS NULL OR datetime(mute_until) > datetime('now'))) AS muted_users,
                (SELECT COUNT(*) FROM status_history
                 WHERE error IS NOT NULL AND error != '')               AS stored_errors,
                (SELECT COUNT(*) FROM user_settings
                 WHERE is_merchant = 1)                                 AS merchant_users,
                (SELECT COUNT(DISTINCT m.user_id)
                 FROM monitoring m
                 LEFT JOIN user_settings s ON s.user_id = m.user_id
                 WHERE COALESCE(s.is_merchant, 0) = 0)                 AS regular_users
            """
        )
        return {key: int(rows[0][key]) for key in rows[0].keys()}

    async def recent_errors(self, limit: int = 10) -> list[aiosqlite.Row]:
        return await self.connection.execute_fetchall(
            """
            SELECT checked_at, url, error
            FROM status_history
            WHERE error IS NOT NULL AND error != ''
            ORDER BY id DESC
            LIMIT ?
            """,
            (limit,),
        )

    async def recent_history(self, limit: int = 10) -> list[aiosqlite.Row]:
        return await self.connection.execute_fetchall(
            """
            SELECT checked_at, server_name, status, raw_status, url
            FROM status_history
            WHERE error IS NULL
            ORDER BY id DESC
            LIMIT ?
            """,
            (limit,),
        )

    async def get_comment(self, user_id: int, url: str) -> str | None:
        """Вернуть комментарий пользователя к серверу или None."""
        rows = await self.connection.execute_fetchall(
            "SELECT comment FROM server_comments WHERE user_id = ? AND url = ?",
            (user_id, url),
        )
        return rows[0]["comment"] if rows else None

    async def set_comment(self, user_id: int, url: str, comment: str) -> None:
        """Сохранить (или обновить) комментарий к серверу."""
        await self.connection.execute(
            """
            INSERT INTO server_comments (user_id, url, comment, updated_at)
            VALUES (?, ?, ?, datetime('now'))
            ON CONFLICT(user_id, url) DO UPDATE SET
                comment = excluded.comment,
                updated_at = datetime('now')
            """,
            (user_id, url, comment[:COMMENT_MAX_LEN]),
        )
        await self.connection.commit()

    async def delete_comment(self, user_id: int, url: str) -> None:
        """Удалить комментарий пользователя к серверу."""
        await self.connection.execute(
            "DELETE FROM server_comments WHERE user_id = ? AND url = ?",
            (user_id, url),
        )
        await self.connection.commit()

    # ------------------------------------------------------------------
    # Ready-watch persistence
    # ------------------------------------------------------------------

    async def save_ready_watch(
        self,
        url: str,
        expires_at: datetime,
        watch_messages: dict[int, tuple[int, str]],
    ) -> None:
        """Сохранить (или обновить) запись окна наблюдения в БД.

        ``watch_messages`` сериализуется в JSON: {str(user_id): [message_id, text]}.
        ``expires_at`` хранится как ISO-строка UTC.
        """
        messages_json = json.dumps(
            {str(uid): [mid, txt] for uid, (mid, txt) in watch_messages.items()}
        )
        await self.connection.execute(
            """
            INSERT INTO ready_watch (url, expires_at, messages)
            VALUES (?, ?, ?)
            ON CONFLICT(url) DO UPDATE SET
                expires_at = excluded.expires_at,
                messages   = excluded.messages
            """,
            (url, expires_at.isoformat(), messages_json),
        )
        await self.connection.commit()

    async def delete_ready_watch(self, url: str) -> None:
        """Удалить запись окна наблюдения из БД."""
        await self.connection.execute("DELETE FROM ready_watch WHERE url = ?", (url,))
        await self.connection.commit()

    async def load_ready_watch(self) -> dict[str, tuple[float, dict[int, tuple[int, str]]]]:
        """Загрузить все активные окна наблюдения из БД.

        Истёкшие записи удаляются прямо при загрузке.
        Возвращает словарь совместимый с ``Monitor._ready_watch``:
        {url: (monotonic_deadline, {user_id: (message_id, text)})}.
        """
        rows = await self.connection.execute_fetchall(
            "SELECT url, expires_at, messages FROM ready_watch"
        )
        now_utc = datetime.now(timezone.utc)
        now_mono = time.monotonic()
        result: dict[str, tuple[float, dict[int, tuple[int, str]]]] = {}
        expired_urls: list[str] = []

        for row in rows:
            try:
                expires_at = datetime.fromisoformat(row["expires_at"])
                if expires_at <= now_utc:
                    expired_urls.append(row["url"])
                    continue
                seconds_left = (expires_at - now_utc).total_seconds()
                deadline = now_mono + seconds_left
                raw: dict[str, list] = json.loads(row["messages"])
                watch_messages: dict[int, tuple[int, str]] = {
                    int(uid): (int(v[0]), str(v[1])) for uid, v in raw.items()
                }
                result[row["url"]] = (deadline, watch_messages)
            except Exception as exc:
                LOG.warning("load_ready_watch: skipping bad entry for %s: %s", row["url"], exc)
                expired_urls.append(row["url"])

        if expired_urls:
            placeholders = ",".join("?" * len(expired_urls))
            await self.connection.execute(
                f"DELETE FROM ready_watch WHERE url IN ({placeholders})", expired_urls
            )
            await self.connection.commit()
            LOG.info("load_ready_watch: pruned %d expired entries", len(expired_urls))

        return result

    async def comments_for_user(self, user_id: int) -> dict[str, str]:
        """Вернуть словарь {url: comment} для всех серверов с комментариями."""
        rows = await self.connection.execute_fetchall(
            "SELECT url, comment FROM server_comments WHERE user_id = ?",
            (user_id,),
        )
        return {row["url"]: row["comment"] for row in rows}

    async def comments_for_url(self, url: str) -> dict[int, str]:
        """Вернуть словарь {user_id: comment} для всех пользователей, следящих за URL.

        Используется монитором вместо N отдельных get_comment-запросов в цикле
        уведомлений: один SELECT вместо одного на каждого подписчика.
        """
        rows = await self.connection.execute_fetchall(
            "SELECT user_id, comment FROM server_comments WHERE url = ?",
            (url,),
        )
        return {row["user_id"]: row["comment"] for row in rows}

    async def get_url_by_subscription_id(self, user_id: int, subscription_id: int) -> str | None:
        """Вернуть URL подписки по её ID (нужно для обработки callback-кнопок)."""
        rows = await self.connection.execute_fetchall(
            "SELECT url FROM monitoring WHERE user_id = ? AND id = ?",
            (user_id, subscription_id),
        )
        return rows[0]["url"] if rows else None

    @property
    def connection(self) -> aiosqlite.Connection:
        if self.db is None:
            raise RuntimeError("Database is not connected")
        return self.db


# ---------------------------------------------------------------------------
# UI helpers
# ---------------------------------------------------------------------------

def status_text(status: str) -> str:
    if status == "ready":
        return "готов"
    if status == "offline":
        return "недоступен"
    return "занят"


def history_status_text(status: str) -> str:
    if status == "ready":
        return "свободен"
    if status == "offline":
        return "недоступен"
    return "занят"


def notify_text(last_status: str, pending_notify: int) -> str:
    """Человекочитаемое описание состояния ожидания для /list."""
    if last_status == "ready":
        return "✅ Можно пользоваться"
    if last_status == "offline":
        return "🔴 Сервер недоступен, ждём возврата в онлайн"
    # busy
    return "⏳ Ждём освобождения"


def waiting_text(waiting_count: int) -> str:
    return f"Ещё ждут освобождения: {pluralize_users(waiting_count)}"


def parse_db_datetime(value: str) -> datetime:
    text = str(value).strip()
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        parsed = datetime.strptime(text, "%Y-%m-%d %H:%M:%S")
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def format_msk_time(value: datetime) -> str:
    return value.astimezone(MOSCOW_TZ).strftime("%H:%M")


def format_duration_ru(seconds: float | int | None) -> str:
    if seconds is None:
        return "нет данных"
    total = max(0, int(round(float(seconds))))
    days, rem = divmod(total, 86_400)
    hours, rem = divmod(rem, 3_600)
    minutes = rem // 60
    parts: list[str] = []
    if days:
        parts.append(f"{days} д")
    if hours:
        parts.append(f"{hours} ч")
    if minutes or not parts:
        parts.append(f"{minutes} мин")
    return " ".join(parts[:3])


def format_remaining(until: datetime) -> str:
    """Форматировать оставшееся время тишины в читаемый вид."""
    delta = until - datetime.now(timezone.utc)
    total = max(0, int(delta.total_seconds()))
    hours, rem = divmod(total, 3600)
    minutes = rem // 60
    if hours and minutes:
        return f"{hours} ч {minutes} мин"
    if hours:
        return f"{hours} ч"
    return f"{minutes} мин"


def delete_keyboard(rows: list[aiosqlite.Row]) -> InlineKeyboardMarkup:
    buttons = [
        [InlineKeyboardButton(text=row["server_name"][:64], callback_data=f"delete:{row['id']}")]
        for row in rows
    ]
    return InlineKeyboardMarkup(inline_keyboard=buttons)


def history_keyboard(rows: list[aiosqlite.Row]) -> InlineKeyboardMarkup:
    buttons = [
        [
            InlineKeyboardButton(
                text=row["server_name"][:64],
                callback_data=f"{HISTORY_CALLBACK_PREFIX}{row['id']}",
            )
        ]
        for row in rows[:MAX_SUBSCRIPTIONS_PER_USER]
    ]
    return InlineKeyboardMarkup(inline_keyboard=buttons)


async def delete_message_safely(message: Message) -> None:
    try:
        await message.delete()
    except TelegramAPIError:
        LOG.debug("Cannot delete bot message", exc_info=True)


async def build_status_text(db: Database, drova: DrovaClient, user_id: int) -> str:
    """Запросить свежие статусы через API и вернуть форматированный текст.

    Используется командой /status для получения актуальных данных.
    Намеренно не пишет в status_history — это зона ответственности Monitor,
    чтобы не дублировать записи при каждом ручном запросе.
    """
    rows = await db.subscriptions_for_user(user_id)
    if not rows:
        return "Подписок пока нет. Пришлите ссылку на станцию Drova."

    waiting_counts = await db.active_waiting_counts_for_user(
        user_id, [row["url"] for row in rows]
    )
    lines = ["Текущий статус серверов:"]
    for index, row in enumerate(rows, start=1):
        try:
            snapshot = await drova.snapshot(row["url"])
        except (httpx.RequestError, httpx.HTTPStatusError, ParserError, ValueError) as exc:
            LOG.warning("Status check failed for %s: %s", row["url"], exc)
            lines.append(f"{index}. {row['server_name']} — не удалось проверить")
            continue
        entry = f"{index}. {snapshot.name} — {status_text(snapshot.status)}"
        if snapshot.status != "ready":
            entry += f"\n{waiting_text(waiting_counts.get(row['url'], 0))}"
        lines.append(entry)
    return "\n".join(lines)


async def build_status_text_cached(db: Database, monitor: "Monitor", user_id: int) -> str:
    """Собрать статус-текст БЕЗ вызовов API.

    Приоритет данных:
    1. Monitor._last_statuses — свежайший снимок из последнего цикла мониторинга.
    2. БД (last_status) — если монитор ещё не успел опросить URL.
    """
    rows = await db.subscriptions_for_user(user_id)
    if not rows:
        return "Подписок пока нет. Пришлите ссылку на станцию Drova."
    waiting_counts = await db.active_waiting_counts_for_user(
        user_id, [row["url"] for row in rows]
    )
    lines = ["Текущий статус серверов:"]
    for index, row in enumerate(rows, start=1):
        snap = monitor._last_statuses.get(row["url"])
        if snap:
            entry = f"{index}. {snap.name} — {status_text(snap.status)}"
            if snap.status != "ready":
                entry += f"\n{waiting_text(waiting_counts.get(row['url'], 0))}"
            lines.append(entry)
        else:
            # Данных в кэше ещё нет — берём из БД
            entry = f"{index}. {row['server_name']} — {status_text(row['last_status'])} (из кэша)"
            if row["last_status"] != "ready":
                entry += f"\n{waiting_text(waiting_counts.get(row['url'], 0))}"
            lines.append(entry)
    return "\n".join(lines)


def _transition_intervals(
    rows: list[aiosqlite.Row], start: datetime, end: datetime
) -> list[StatusInterval]:
    if end <= start or not rows:
        return []

    parsed = [
        (
            row["from_status"],
            row["to_status"],
            parse_db_datetime(row["changed_at"]),
        )
        for row in rows
    ]
    parsed.sort(key=lambda item: item[2])

    intervals: list[StatusInterval] = []
    current_status: Status | None = None
    current_start = start
    previous_status: Status | None = None

    first_from, first_to, first_at = parsed[0]
    if first_at < start:
        current_status = first_to
        previous_status = first_from
        transition_iter = parsed[1:]
    elif first_at == start:
        current_status = first_to
        previous_status = first_from
        transition_iter = parsed[1:]
    else:
        transition_iter = parsed

    for from_status, to_status, changed_at in transition_iter:
        if changed_at < start:
            current_status = to_status
            previous_status = from_status
            current_start = start
            continue
        if changed_at > end:
            break
        if current_status is None:
            current_status = to_status
            previous_status = from_status
            current_start = max(changed_at, start)
            continue
        if changed_at > current_start:
            intervals.append(
                StatusInterval(
                    status=current_status,
                    start=current_start,
                    end=changed_at,
                    previous_status=previous_status,
                    next_status=to_status,
                )
            )
        previous_status = current_status
        current_status = to_status
        current_start = max(changed_at, start)

    if current_status is not None and end > current_start:
        intervals.append(
            StatusInterval(
                status=current_status,
                start=current_start,
                end=end,
                previous_status=previous_status,
                next_status=None,
            )
        )
    return intervals


def _average_seconds(values: list[float]) -> float | None:
    if not values:
        return None
    return sum(values) / len(values)


def _public_history_lines(rows: list[aiosqlite.Row], window_start: datetime) -> list[str]:
    """Сформировать публичные строки истории с состоянием на начало окна."""
    if not rows:
        return ["Данных по статусу за период пока нет."]

    lines: list[str] = []
    for index, row in enumerate(rows):
        changed_at = parse_db_datetime(row["changed_at"])
        status = history_status_text(row["to_status"])
        if index == 0 and changed_at < window_start:
            lines.append(f"{format_msk_time(window_start)} - {status} (состояние на начало периода)")
            continue
        if changed_at >= window_start:
            lines.append(f"{format_msk_time(changed_at)} - {status}")

    return lines or ["Данных по статусу за период пока нет."]


async def build_history_text(
    db: Database,
    url: str,
    *,
    include_private_stats: bool,
) -> str:
    server = await db.server_by_url(url)
    if server is None:
        return "Такой сервер не находится в базе данных."

    now_utc = datetime.now(timezone.utc)
    last_day_start = now_utc - timedelta(hours=HISTORY_PUBLIC_HOURS)
    public_rows = await db.transition_rows_for_window(url, last_day_start, now_utc)

    lines = [
        f"История сервера: {server['server_name']}",
        url,
        "",
        f"Переходы статуса за последние {HISTORY_PUBLIC_HOURS} часа:",
    ]
    lines.extend(_public_history_lines(public_rows, last_day_start))

    if not include_private_stats:
        return "\n".join(lines)

    now_msk = now_utc.astimezone(MOSCOW_TZ)
    seven_start_msk = (now_msk.date() - timedelta(days=HISTORY_STATS_DAYS - 1))
    seven_start = datetime.combine(
        seven_start_msk,
        datetime.min.time(),
        tzinfo=MOSCOW_TZ,
    ).astimezone(timezone.utc)
    rows = await db.transition_rows_for_window(url, seven_start, now_utc)
    seven_intervals = _transition_intervals(rows, seven_start, now_utc)
    day_intervals = _transition_intervals(rows, last_day_start, now_utc)

    busy_durations = [
        (interval.end - interval.start).total_seconds()
        for interval in seven_intervals
        if interval.status == "busy"
    ]
    ready_durations_7d = [
        (interval.end - interval.start).total_seconds()
        for interval in seven_intervals
        if interval.status == "ready"
    ]
    ready_durations_24h = [
        (interval.end - interval.start).total_seconds()
        for interval in day_intervals
        if interval.status == "ready"
    ]
    completed_idle_between_sessions = [
        (interval.end - interval.start).total_seconds()
        for interval in seven_intervals
        if interval.status == "ready" and interval.next_status == "busy"
    ]

    lines.extend(
        [
            "",
            f"Статистика за последние {HISTORY_STATS_DAYS} календарных дней:",
            f"Среднее время сессии: {format_duration_ru(_average_seconds(busy_durations))}",
            f"Общее время простоя: {format_duration_ru(sum(ready_durations_7d))}",
            f"Общее время простоя за последние 24 часа: {format_duration_ru(sum(ready_durations_24h))}",
            "Среднее время простоя между сессиями: "
            f"{format_duration_ru(_average_seconds(completed_idle_between_sessions))}",
        ]
    )
    return "\n".join(lines)


async def build_unsilenced_status_text(db: Database, monitor: "Monitor", user_id: int) -> str:
    status = await build_status_text(db, monitor.drova, user_id)
    return (
        "Режим тишины отключен. Ниже текущие состояния серверов, "
        "чтобы вы не пропустили изменения за время паузы.\n\n"
        f"{status}"
    )


def command_arg(text: str | None) -> str:
    if not text:
        return ""
    parts = text.strip().split(maxsplit=1)
    return parts[1].strip() if len(parts) > 1 else ""


def is_admin(user_id: int, settings: Settings) -> bool:
    return user_id in settings.admin_ids


async def register_bot_commands(bot: Bot, settings: Settings) -> None:
    await bot.set_my_commands(BOT_COMMANDS)
    for admin_id in settings.admin_ids:
        await bot.set_my_commands(ADMIN_BOT_COMMANDS, scope=BotCommandScopeChat(chat_id=admin_id))


# ---------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------

router = Router()


@router.message(CommandStart())
async def start(message: Message, db: Database) -> None:
    if not message.from_user:
        return
    await message.answer(
        "Пришлите ссылку на станцию Drova вида:\n"
        "https://drova.io/stations/...\n\n"
        "Можно добавить до 5 подписок. Я проверяю уникальные ссылки по очереди "
        "и пишу при переходе из «занят» в «готов», а также когда сервер "
        "возвращается из офлайна.\n\n"
        "Команды:\n"
        "/status — статусы серверов\n"
        "/list — мои подписки\n"
        "/history — история и статистика сервера\n"
        "/delete — удалить подписку кнопкой\n"
        "/unsubscribe 1 — удалить подписку по номеру из /list\n"
        "/remove — удалить подписку по ссылке\n"
        "/clear — удалить все мои подписки\n"
        "/silence — режим тишины\n"
        "/role — сменить роль (пользователь / мерчант)\n"
        "/export — выгрузить подписки в файл\n"
        "/import_subs — импортировать подписки\n"
        "/donate — поддержать проект\n"
        "/help — подробная справка",
        reply_markup=MAIN_KEYBOARD,
    )
    role_already_set = await db.has_role_selected(message.from_user.id)
    if role_already_set:
        is_merchant = await db.get_user_role(message.from_user.id)
        role_label = "🏪 Мерчант" if is_merchant else "👤 Пользователь"
        await message.answer(
            f"Ваша текущая роль: {role_label}. Сменить можно через /role.",
        )
    else:
        await message.answer(
            "Кто вы?\n\n"
            "👤 Пользователь — уведомления приходят, когда сервер свободен "
            "и открыт для всех.\n\n"
            "🏪 Мерчант — дополнительно уведомляет, когда сервер свободен, "
            "но ещё скрыт для обычных пользователей.\n\n"
            "По умолчанию установлен статус «Пользователь». "
            "Сменить роль можно в любой момент через /role.",
            reply_markup=ROLE_SELECT_KEYBOARD,
        )


@router.message(Command("help"))
async def help_command(message: Message) -> None:
    """Подробная справка — отдельный текст, не дублирует /start."""
    await message.answer(
        "Справка по командам бота:\n\n"
        "/start — краткая инструкция и приветствие\n"
        "/list — список ваших подписок с текущим статусом каждой и комментариями\n"
        "/comments — управление комментариями к серверам (добавить, изменить, удалить)\n"
        "/status — проверить серверы прямо сейчас (данные кешируются на 60 с)\n"
        "/history — история сервера: /history 1 или /history <ссылка>\n"
        "/delete — выбрать подписку для удаления через кнопку\n"
        "/unsubscribe N — удалить подписку по её номеру из /list (например /unsubscribe 2)\n"
        "/remove — удалить подписку по ссылке (можно вставить URL прямо в команду)\n"
        "/clear — удалить все подписки сразу (попросит подтверждение)\n"
        "/silence — выключить уведомления на 3 ч, 8 ч, 24 ч или до отмены\n"
        "/role — сменить роль (пользователь / мерчант)\n"
        "/export — выгрузить список подписок в текстовый файл\n"
        "/import_subs — импортировать подписки из текста или .txt-файла\n"
        "/donate — поддержать проект (USDT, СБП, CloudTips)\n\n"
        "Роли пользователей:\n"
        "👤 Пользователь — уведомления приходят только когда сервер свободен "
        "и открыт для всех (published=true).\n"
        "🏪 Мерчант — дополнительно получает уведомление когда сервер свободен, "
        "но ещё скрыт для обычных пользователей (published=false). "
        "Удобно для владельцев серверов.\n\n"
        "Комментарии к серверам:\n"
        "После добавления подписки бот предложит создать комментарий. "
        "Управлять комментариями также можно через /list или /comments — кнопки под сообщением.\n"
        "Комментарии отображаются в /list и в уведомлениях об освобождении.\n\n"
        "История сервера:\n"
        "/history без аргументов покажет кнопки ваших серверов. "
        "Обычные пользователи видят переходы статуса за последние 24 часа. "
        "Админ и мерчанты дополнительно видят среднюю длительность сессии, "
        "общее время простоя и средний простой между сессиями за последние 7 календарных дней.\n\n"
        "Как добавить подписку:\n"
        "Просто пришлите ссылку вида https://drova.io/stations/<uuid>.\n\n"
        "Когда приходит уведомление:\n"
        "• Сервер перешёл из «занят» в «готов»\n"
        "• Сервер вернулся из офлайна и снова доступен\n"
        "• (Мерчанты) Сервер свободен, но скрыт для обычных пользователей\n\n"
        "Статусы сервера:\n"
        "✅ Готов — можно подключаться\n"
        "⏳ Занят — кто-то уже в сессии\n"
        "🔴 Недоступен (offline) — сервер временно не отвечает\n"
        "🔒 Свободен, но скрыт — published=false (уведомление только для мерчантов)\n\n"
        "Лимит: до 5 подписок на аккаунт.",
        reply_markup=MAIN_KEYBOARD,
    )


@router.message(Command("donate"))
async def donate_command(message: Message) -> None:
    """Показать реквизиты для поддержки проекта."""
    await message.answer(DONATE_TEXT, parse_mode="MarkdownV2")


async def build_comment_keyboard(
    db: Database, user_id: int
) -> InlineKeyboardMarkup | None:
    """Построить inline-клавиатуру управления комментариями для пользователя.

    Возвращает None, если подписок нет.
    """
    rows = await db.subscriptions_for_user(user_id)
    if not rows:
        return None
    comments = await db.comments_for_user(user_id)
    buttons = []
    for row in rows:
        has_comment = row["url"] in comments
        label = f"{'✏️' if has_comment else '➕'} {row['server_name'][:30]}"
        buttons.append([InlineKeyboardButton(
            text=label,
            callback_data=f"comment:{'edit' if has_comment else 'add'}:{row['id']}",
        )])
        if has_comment:
            buttons[-1].append(InlineKeyboardButton(
                text="🗑",
                callback_data=f"comment:del:{row['id']}",
            ))
    return InlineKeyboardMarkup(inline_keyboard=buttons)


async def build_list_text(db: Database, user_id: int) -> str | None:
    """Построить текст для /list. Возвращает None, если подписок нет."""
    rows = await db.subscriptions_for_user(user_id)
    if not rows:
        return None
    muted = await db.user_muted(user_id)
    comments = await db.comments_for_user(user_id)
    waiting_counts = await db.active_waiting_counts_for_user(
        user_id, [row["url"] for row in rows]
    )
    lines = [
        f"Ваши подписки: {len(rows)}/{MAX_SUBSCRIPTIONS_PER_USER}",
        f"Режим тишины: {'включён' if muted else 'выключен'}",
    ]
    for index, row in enumerate(rows, start=1):
        comment = comments.get(row["url"])
        comment_line = f"💬 {comment}" if comment else ""
        entry = (
            f"{index}. {row['server_name']} — {status_text(row['last_status'])}\n"
            f"{notify_text(row['last_status'], row['pending_notify'])}\n"
        )
        if row["last_status"] != "ready":
            entry += f"{waiting_text(waiting_counts.get(row['url'], 0))}\n"
        if comment_line:
            entry += f"{comment_line}\n"
        entry += row["url"]
        lines.append(entry)
    return "\n\n".join(lines)


@router.message(Command("list"))
async def list_command(message: Message, db: Database) -> None:
    if not message.from_user:
        return
    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
    text = await build_list_text(db, message.from_user.id)
    if text is None:
        await message.answer(
            "Подписок пока нет. Пришлите ссылку на станцию Drova.",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    await message.answer(text, reply_markup=LIST_REFRESH_KEYBOARD)


@router.callback_query(F.data == "list:refresh")
async def list_refresh_callback(callback: CallbackQuery, db: Database) -> None:
    if not callback.from_user or not callback.message:
        return
    remaining = _check_refresh_cooldown(callback.from_user.id)
    if remaining is not None:
        await callback.answer(f"Подождите {remaining:.0f} сек")
        return
    text = await build_list_text(db, callback.from_user.id)
    if text is None:
        await callback.answer("Подписок нет", show_alert=True)
        return
    try:
        await callback.message.edit_text(text, reply_markup=LIST_REFRESH_KEYBOARD)
    except TelegramAPIError as exc:
        LOG.warning("Cannot refresh list message: %s", exc)
    await callback.answer("Обновлено")


@router.message(Command("comments"))
async def comments_command(message: Message, db: Database) -> None:
    """Управление комментариями к серверам — отдельная команда без вывода всего /list."""
    if not message.from_user:
        return
    keyboard = await build_comment_keyboard(db, message.from_user.id)
    if not keyboard:
        await message.answer(
            "Подписок пока нет. Пришлите ссылку на станцию Drova.",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    await message.answer("Комментарии к серверам:", reply_markup=keyboard)


@router.message(Command("status"))
async def status_command(message: Message, db: Database, drova: DrovaClient) -> None:
    if not message.from_user:
        return
    # Per-user lock: предотвращаем гонку, если пользователь быстро жмёт /status
    user_lock = get_user_status_lock(message.from_user.id)
    if user_lock.locked():
        await message.answer(
            "Предыдущий запрос ещё выполняется, подождите немного.",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    async with user_lock:
        await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
        text = await build_status_text(db, drova, message.from_user.id)
        await message.answer(text, reply_markup=STATUS_REFRESH_KEYBOARD)
    release_user_status_lock(message.from_user.id, user_lock)


@router.callback_query(F.data == "status:refresh")
async def status_refresh_callback(
    callback: CallbackQuery, db: Database, monitor: "Monitor"
) -> None:
    if not callback.from_user or not callback.message:
        return
    remaining = _check_refresh_cooldown(callback.from_user.id)
    if remaining is not None:
        await callback.answer(f"Подождите {remaining:.0f} сек")
        return
    user_lock = get_user_status_lock(callback.from_user.id)
    if user_lock.locked():
        await callback.answer("Подождите, идёт обновление…")
        return
    async with user_lock:
        text = await build_status_text_cached(db, monitor, callback.from_user.id)
        try:
            await callback.message.edit_text(text, reply_markup=STATUS_REFRESH_KEYBOARD)
        except TelegramAPIError as exc:
            LOG.warning("Cannot refresh status message: %s", exc)
        await callback.answer("Обновлено")
    release_user_status_lock(callback.from_user.id, user_lock)


@router.message(Command("history"))
async def history_command(message: Message, db: Database, settings: Settings) -> None:
    if not message.from_user:
        return
    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
    arg = command_arg(message.text)
    include_private_stats = (
        is_admin(message.from_user.id, settings)
        or await db.get_user_role(message.from_user.id)
    )

    if not arg:
        rows = await db.subscriptions_for_user(message.from_user.id)
        if not rows:
            await message.answer(
                "Подписок пока нет. Пришлите ссылку на станцию Drova.",
                reply_markup=MAIN_KEYBOARD,
            )
            return
        await message.answer("Выберите сервер для просмотра истории:", reply_markup=history_keyboard(rows))
        return

    url: str | None = None
    if arg.isdigit():
        number = int(arg)
        rows = await db.subscriptions_for_user(message.from_user.id)
        if number < 1 or number > len(rows):
            await message.answer(
                f"Нет подписки с номером {number}. Проверьте список через /list.",
                reply_markup=MAIN_KEYBOARD,
            )
            return
        url = rows[number - 1]["url"]
    else:
        url = extract_url(arg)
        if url is None:
            await message.answer(
                "Не удалось распознать сервер. Используйте /history N или /history <ссылка>.",
                reply_markup=MAIN_KEYBOARD,
            )
            return

    text = await build_history_text(db, url, include_private_stats=include_private_stats)
    await message.answer(text, reply_markup=MAIN_KEYBOARD)


@router.callback_query(F.data.startswith("history:"))
async def history_callback(callback: CallbackQuery, db: Database, settings: Settings) -> None:
    if not callback.from_user or not callback.data:
        return
    try:
        subscription_id = int(callback.data.split(":", 1)[1])
    except ValueError:
        await callback.answer("Некорректная кнопка", show_alert=True)
        return

    url = await db.get_url_by_subscription_id(callback.from_user.id, subscription_id)
    if not url:
        await callback.answer("Подписка не найдена", show_alert=True)
        return

    include_private_stats = (
        is_admin(callback.from_user.id, settings)
        or await db.get_user_role(callback.from_user.id)
    )
    text = await build_history_text(db, url, include_private_stats=include_private_stats)
    if callback.message:
        await callback.message.edit_text(text, reply_markup=None)
    await callback.answer()


@router.message(Command("delete"))
async def delete_command(message: Message, db: Database) -> None:
    if not message.from_user:
        return
    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
    rows = await db.subscriptions_for_user(message.from_user.id)
    if not rows:
        await message.answer("Удалять нечего: подписок пока нет.", reply_markup=MAIN_KEYBOARD)
        return
    await message.answer("Выберите подписку для удаления:", reply_markup=delete_keyboard(rows))


@router.callback_query(F.data.startswith("delete:"))
async def delete_callback(callback: CallbackQuery, db: Database) -> None:
    if not callback.from_user or not callback.data:
        return
    try:
        subscription_id = int(callback.data.split(":", 1)[1])
    except ValueError:
        await callback.answer("Некорректная кнопка", show_alert=True)
        return

    deleted = await db.remove_by_id(callback.from_user.id, subscription_id)
    await callback.answer("Удалено" if deleted else "Подписка уже удалена")
    if callback.message:
        rows = await db.subscriptions_for_user(callback.from_user.id)
        if rows:
            await callback.message.edit_text(
                "Выберите подписку для удаления:", reply_markup=delete_keyboard(rows)
            )
        else:
            await callback.message.edit_text("Подписок больше нет.")


@router.message(Command("silence"))
async def silence_command(message: Message, db: Database) -> None:
    if not message.from_user:
        return
    is_muted, until = await db.get_mute_info(message.from_user.id)
    if is_muted:
        if until is not None:
            remaining = format_remaining(until)
            text = f"🔕 Тишина включена. Осталось: {remaining}. Хотите включить уведомления?"
        else:
            text = "🔕 Тишина включена бессрочно. Хотите включить уведомления?"
        await message.answer(text, reply_markup=SILENCE_OFF_KEYBOARD)
    else:
        await message.answer(
            "🔕 На какое время выключить уведомления?",
            reply_markup=SILENCE_ON_KEYBOARD,
        )


@router.callback_query(F.data.startswith("silence:"))
async def silence_callback(callback: CallbackQuery, db: Database, monitor: "Monitor") -> None:
    if not callback.from_user or not callback.data:
        return
    action = callback.data.split(":", 1)[1]

    if action == "off":
        await db.unmute(callback.from_user.id)
        text = await build_unsilenced_status_text(db, monitor, callback.from_user.id)
    else:
        hours_map = {"3h": 3, "8h": 8, "24h": 24}
        if action in hours_map:
            until = datetime.now(timezone.utc) + timedelta(hours=hours_map[action])
            await db.set_mute(callback.from_user.id, until)
            label = f"{hours_map[action]} ч"
            text = f"🔕 Тишина включена на {label}. Подписки сохранены."
        elif action == "forever":
            await db.set_mute(callback.from_user.id, until=None)
            text = "🔕 Тишина включена до вашей отмены. Подписки сохранены."
        else:
            await callback.answer("Неизвестное действие", show_alert=True)
            return

    await callback.answer()
    if callback.message:
        reply_markup = STATUS_REFRESH_KEYBOARD if action == "off" else None
        await callback.message.edit_text(text, reply_markup=reply_markup)


@router.message(Command("role"))
async def role_command(message: Message, db: Database) -> None:
    """Показать текущую роль и предложить сменить."""
    if not message.from_user:
        return
    is_merchant = await db.get_user_role(message.from_user.id)
    if is_merchant:
        await message.answer(
            "Ваша текущая роль: 🏪 Мерчант\n\n"
            "Вы получаете уведомления когда сервер освободился, в том числе "
            "когда он ещё скрыт для обычных пользователей.\n\n"
            "Хотите сменить роль на «Пользователь»?",
            reply_markup=ROLE_CHANGE_TO_USER_KEYBOARD,
        )
    else:
        await message.answer(
            "Ваша текущая роль: 👤 Пользователь\n\n"
            "Вы получаете уведомления только когда сервер свободен "
            "и открыт для всех.\n\n"
            "Хотите сменить роль на «Мерчант»?",
            reply_markup=ROLE_CHANGE_TO_MERCHANT_KEYBOARD,
        )


@router.callback_query(F.data.startswith("role:"))
async def role_callback(callback: CallbackQuery, db: Database) -> None:
    """Обработка выбора / смены роли пользователя."""
    if not callback.from_user or not callback.data:
        return
    action = callback.data.split(":", 1)[1]
    if action == "merchant":
        await db.set_user_role(callback.from_user.id, is_merchant=True)
        text = (
            "✅ Роль установлена: 🏪 Мерчант\n\n"
            "Вы будете получать уведомления когда сервер освободился, "
            "даже если он ещё скрыт для обычных пользователей."
        )
    elif action == "user":
        await db.set_user_role(callback.from_user.id, is_merchant=False)
        text = (
            "✅ Роль установлена: 👤 Пользователь\n\n"
            "Вы будете получать уведомления только когда сервер свободен "
            "и открыт для всех."
        )
    else:
        await callback.answer("Неизвестная роль", show_alert=True)
        return

    await callback.answer()
    if callback.message:
        await callback.message.edit_text(text)


# ---------------------------------------------------------------------------
# Export / Import handlers
# ---------------------------------------------------------------------------

@router.message(Command("export"))
async def export_command(message: Message, db: Database) -> None:
    """Экспортировать список подписок как текстовый файл (URL, по одному на строку)."""
    if not message.from_user:
        return
    rows = await db.subscriptions_for_user(message.from_user.id)
    if not rows:
        await message.answer("Подписок нет — экспортировать нечего.", reply_markup=MAIN_KEYBOARD)
        return

    content = "\n".join(row["url"] for row in rows)
    filename = f"drova_subscriptions_{message.from_user.id}.txt"
    file_bytes = content.encode("utf-8")

    await message.answer_document(
        BufferedInputFile(file_bytes, filename=filename),
        caption=(
            f"Ваши подписки ({len(rows)} шт.). "
            "Чтобы импортировать на другом аккаунте — используйте /import_subs."
        ),
        reply_markup=MAIN_KEYBOARD,
    )


@router.message(Command("import_subs"))
async def import_command(message: Message, db: Database, state: FSMContext) -> None:
    """Начать импорт подписок: переходим в FSM-состояние ожидания списка URL."""
    if not message.from_user:
        return
    count = await db.subscription_count(message.from_user.id)
    if count >= MAX_SUBSCRIPTIONS_PER_USER:
        await message.answer(
            f"Лимит подписок ({MAX_SUBSCRIPTIONS_PER_USER}) исчерпан. "
            "Удалите лишние через /delete, затем повторите импорт.",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    await state.set_state(ImportStates.waiting_for_urls)
    await message.answer(
        f"Пришлите список ссылок Drova для импорта — по одной на строке "
        f"(или через пробел). Можно прислать несколько сразу.\n\n"
        f"Также можно прикрепить текстовый файл .txt, экспортированный через /export.\n\n"
        f"Сейчас у вас {count}/{MAX_SUBSCRIPTIONS_PER_USER} подписок. "
        "Отправьте /cancel чтобы отменить.",
        reply_markup=MAIN_KEYBOARD,
    )


@router.message(ImportStates.waiting_for_urls, Command("cancel"))
async def import_cancel(message: Message, state: FSMContext) -> None:
    await state.clear()
    await message.answer("Импорт отменён.", reply_markup=MAIN_KEYBOARD)


def _extract_unique_urls(text: str) -> list[str]:
    """Извлечь уникальные нормализованные Drova-ссылки из произвольного текста."""
    found: list[str] = []
    seen: set[str] = set()
    for raw in URL_RE.findall(text):
        norm = normalize_url(raw)
        if norm and norm not in seen:
            seen.add(norm)
            found.append(norm)
    return found


async def _execute_import(
    message: Message,
    db: Database,
    drova: DrovaClient,
    found_urls: list[str],
    source: str,
) -> None:
    """Общая логика импорта списка Drova-ссылок.

    Выполняет:
    - Pre-flight проверку лимита: если новых ссылок больше, чем свободных
      мест, весь запрос отклоняется (с учётом того, что ссылки, на которые
      пользователь уже подписан, слот не занимают — они будут обновлены).
    - Rate-limit: пропускает URL, недавно обработанные тем же пользователем.
    - Параллельный fetch через Drova API (до 5 одновременных запросов).
    - Последовательную запись в БД (исключает гонку при подсчёте подписок).
    - Подробный отчёт (включая списки URL для каждой категории пропущенных).

    ``source`` используется только в лог-сообщениях («text» / «file»).
    """
    assert message.from_user  # гарантируется вызывающими хендлерами
    user_id = message.from_user.id

    # Получаем текущие подписки пользователя
    rows = await db.subscriptions_for_user(user_id)
    existing_urls: set[str] = {row["url"] for row in rows}
    current_count = len(rows)
    free_slots = MAX_SUBSCRIPTIONS_PER_USER - current_count

    # Разделяем ссылки на новые (займут слот) и уже существующие (обновление)
    new_urls = [u for u in found_urls if u not in existing_urls]

    # Pre-flight: проверяем, что новых ссылок не больше, чем свободных мест
    if current_count >= MAX_SUBSCRIPTIONS_PER_USER:
        subs_list = "\n".join(f"  {i}. {row['server_name']}" for i, row in enumerate(rows, 1))
        await message.answer(
            f"Лимит подписок ({MAX_SUBSCRIPTIONS_PER_USER}) уже исчерпан.\n"
            f"Ваши текущие подписки:\n{subs_list}\n\n"
            "Удалите лишние через /delete, затем повторите импорт.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    if len(new_urls) > free_slots:
        await message.answer(
            f"В списке {len(new_urls)} новых ссылок, а свободных мест — {free_slots} "
            f"(у вас {current_count}/{MAX_SUBSCRIPTIONS_PER_USER} подписок).\n\n"
            f"Сократите список до {free_slots} новых ссылок или освободите место "
            "через /delete и повторите импорт.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]

    # --- Фаза 1: rate-limit и параллельный fetch ---
    # Разбираем rate-limit синхронно (до asyncio.gather), чтобы не было
    # гонки при одновременном чтении/записи _subscribe_rate_cache.
    now = time.monotonic()
    skipped_ratelimit: list[str] = []
    urls_to_fetch: list[str] = []

    async with _subscribe_rate_lock:
        _evict_rate_cache(now)
        # Та же логика, что и в text_message — import-путь тоже может
        # накапливать записи при массовом импорте от многих пользователей.
        for url in found_urls:
            cache_key = (user_id, url)
            last_sent = _subscribe_rate_cache.get(cache_key, 0.0)
            if now - last_sent < SUBSCRIBE_RATE_LIMIT_SECONDS:
                skipped_ratelimit.append(url)
            else:
                _subscribe_rate_cache[cache_key] = now
                urls_to_fetch.append(url)

    # Параллельный fetch с ограничением одновременных запросов
    sem = asyncio.Semaphore(5)
    fetch_results: dict[str, Snapshot | Exception] = {}

    async def _fetch_one(url: str) -> None:
        async with sem:
            try:
                fetch_results[url] = await drova.snapshot(url)
            except (httpx.RequestError, httpx.HTTPStatusError, ParserError, ValueError) as exc:
                LOG.warning("Import (%s): failed to fetch %s: %s", source, url, exc)
                fetch_results[url] = exc
                # Сбрасываем rate-limit при ошибке — пользователь сможет сразу повторить
                async with _subscribe_rate_lock:
                    _subscribe_rate_cache.pop((user_id, url), None)

    await asyncio.gather(*[_fetch_one(url) for url in urls_to_fetch])

    # --- Фаза 2: последовательная запись в БД ---
    # Последовательность важна: db.add_subscription проверяет счётчик подписок
    # внутри Python-кода, без БД-уровневого ограничения. Параллельные вызовы
    # могли бы одновременно пройти проверку и суммарно превысить лимит.
    added: list[str] = []
    updated: list[str] = []
    skipped_limit: list[str] = []   # страховка; при корректном pre-flight не должна заполняться
    failed: list[str] = []

    for url in urls_to_fetch:
        result = fetch_results.get(url)
        if result is None or isinstance(result, Exception):
            failed.append(url)
            continue
        snapshot: Snapshot = result  # type: ignore[assignment]
        db_result = await db.add_subscription(user_id, url, snapshot.name, snapshot.status)
        if db_result == "added":
            added.append(snapshot.name)
        elif db_result == "updated":
            updated.append(snapshot.name)
        elif db_result == "limit":
            # Не должно случиться при правильном pre-flight, но обрабатываем для надёжности
            skipped_limit.append(url)
            break

    # --- Отчёт ---
    _MAX_LISTED = 5  # максимум URL выводим списком, остальные суммируем

    def _url_list(urls: list[str]) -> str:
        visible = urls[:_MAX_LISTED]
        rest = len(urls) - len(visible)
        lines = [f"  • {u}" for u in visible]
        if rest:
            lines.append(f"  …и ещё {rest}")
        return "\n".join(lines)

    report_lines: list[str] = []
    if added:
        report_lines.append(f"✅ Добавлено ({len(added)}): " + ", ".join(added))
    if updated:
        report_lines.append(f"🔄 Обновлено ({len(updated)}): " + ", ".join(updated))
    if skipped_limit:
        report_lines.append(
            f"⚠️ Лимит подписок ({MAX_SUBSCRIPTIONS_PER_USER}) достигнут — "
            f"пропущены ({len(skipped_limit)}):\n{_url_list(skipped_limit)}"
        )
    if skipped_ratelimit:
        report_lines.append(
            f"⏱ Слишком частый запрос — пропущены ({len(skipped_ratelimit)}):\n"
            f"{_url_list(skipped_ratelimit)}\n"
            f"Повторите через {int(SUBSCRIBE_RATE_LIMIT_SECONDS)} с."
        )
    if failed:
        report_lines.append(f"❌ Не удалось получить статус ({len(failed)} URL) — пропущены.")
    if not report_lines:
        report_lines.append("Ничего не изменилось.")

    await message.answer("\n".join(report_lines), reply_markup=MAIN_KEYBOARD)


@router.message(ImportStates.waiting_for_urls, F.text)
async def import_urls_received(
    message: Message, db: Database, drova: DrovaClient, state: FSMContext
) -> None:
    """Получаем текст с URL-ами, парсим и запускаем общую логику импорта."""
    await state.clear()
    if not message.from_user or not message.text:
        return

    found_urls = _extract_unique_urls(message.text)
    if not found_urls:
        await message.answer(
            "Не найдено ни одной корректной ссылки Drova. "
            "Ссылки должны иметь вид https://drova.io/stations/<uuid>.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    await _execute_import(message, db, drova, found_urls, source="text")


@router.message(ImportStates.waiting_for_urls, F.document)
async def import_file_received(
    message: Message, db: Database, drova: DrovaClient, state: FSMContext
) -> None:
    """Получаем .txt-файл со списком URL-ов и запускаем общую логику импорта.

    Если документ не является текстовым файлом — сообщаем об ошибке и
    остаёмся в FSM-состоянии, чтобы пользователь мог попробовать снова.
    Если файл корректный (text/plain или расширение .txt) — обрабатываем
    и в любом случае сбрасываем состояние после завершения.
    """
    if not message.from_user or not message.document:
        return

    doc = message.document
    mime_ok = (doc.mime_type or "").startswith("text/plain")
    name_ok = (doc.file_name or "").lower().endswith(".txt")

    if not mime_ok and not name_ok:
        # Невалидный тип файла — остаёмся в состоянии, пользователь может попробовать снова
        await message.answer(
            "⚠️ Ожидается текстовый файл (.txt). "
            "Прикрепите файл, экспортированный через /export, "
            "или отправьте ссылки текстом. Для отмены — /cancel.",
        )
        return

    # Читаем содержимое файла
    try:
        tg_file = await message.bot.get_file(doc.file_id)  # type: ignore[union-attr]
        bio = await message.bot.download_file(tg_file.file_path)  # type: ignore[union-attr]
        text = bio.read().decode("utf-8", errors="replace")  # type: ignore[union-attr]
    except Exception as exc:
        LOG.warning("Import (file): failed to download document: %s", exc)
        await state.clear()
        await message.answer(
            "❌ Не удалось загрузить файл. Попробуйте ещё раз или отправьте ссылки текстом.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    # Файл прочитан успешно — сбрасываем состояние
    await state.clear()

    found_urls = _extract_unique_urls(text)
    if not found_urls:
        await message.answer(
            "Не найдено ни одной корректной ссылки Drova. "
            "Файл должен содержать ссылки вида https://drova.io/stations/<uuid> "
            "по одной на строку.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    await _execute_import(message, db, drova, found_urls, source="file")


@router.message(Command("admin"))
async def admin_command(message: Message, db: Database, settings: Settings) -> None:
    if not message.from_user:
        return
    if not is_admin(message.from_user.id, settings):
        await message.answer("Команда доступна только админу.", reply_markup=MAIN_KEYBOARD)
        return

    stats   = await db.admin_stats()
    errors  = await db.recent_errors()
    history = await db.recent_history(5)

    lines = [
        "Админ-статистика:",
        f"Пользователей с подписками: {stats['users']}",
        f"  из них обычных: {stats['regular_users']}",
        f"  из них мерчантов: {stats['merchant_users']}",
        f"Подписок: {stats['subscriptions']}",
        f"Уникальных URL: {stats['unique_urls']}",
        f"Пользователей в тишине: {stats['muted_users']}",
        f"Ошибок в истории: {stats['stored_errors']}",
    ]

    if errors:
        lines.append("\nПоследние ошибки:")
        for row in errors:
            lines.append(f"- {row['checked_at']} | {row['url']} | {row['error']}")
    else:
        lines.append("\nОшибок нет.")

    if history:
        lines.append("\nПоследние статусы:")
        for row in history:
            lines.append(
                f"- {row['checked_at']} | {row['server_name']} | "
                f"{row['status']} | {row['raw_status']}"
            )

    await message.answer("\n".join(lines), reply_markup=MAIN_KEYBOARD)


@router.message(Command("unsubscribe"))
async def unsubscribe_command(message: Message, db: Database) -> None:
    """Удалить подписку по её порядковому номеру из /list (например /unsubscribe 2)."""
    if not message.from_user:
        return
    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
    text = message.text or ""
    parts = text.split(maxsplit=1)
    number_str = parts[1].strip() if len(parts) > 1 else ""
    if not number_str.isdigit() or int(number_str) < 1:
        await message.answer(
            "Укажите номер подписки из /list, например: /unsubscribe 2",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    number = int(number_str)
    rows = await db.subscriptions_for_user(message.from_user.id)
    if not rows:
        await message.answer("Удалять нечего: подписок пока нет.", reply_markup=MAIN_KEYBOARD)
        return
    if number > len(rows):
        await message.answer(
            f"Нет подписки с номером {number}. У вас {len(rows)} подписок(и). Список: /list",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    row = rows[number - 1]
    deleted = await db.remove_by_id(message.from_user.id, row["id"])
    if deleted:
        await message.answer(f"Подписка #{number} удалена: {row['server_name']}", reply_markup=MAIN_KEYBOARD)
    else:
        await message.answer("Подписка уже была удалена.", reply_markup=MAIN_KEYBOARD)


@router.message(Command("remove"))
async def remove_command(message: Message, db: Database, state: FSMContext) -> None:
    """Удалить подписку по ссылке. Если ссылка не указана — запрашивает через FSM."""
    if not message.from_user:
        return
    url = extract_url(message.text)
    if not url:
        rows = await db.subscriptions_for_user(message.from_user.id)
        if not rows:
            await message.answer("Удалять нечего: подписок пока нет.", reply_markup=MAIN_KEYBOARD)
            return
        await state.set_state(RemoveStates.waiting_for_url)
        await message.answer(
            "Отправьте ссылку на станцию, которую хотите удалить\n"
            "(или /list чтобы посмотреть подписки, /unsubscribe N чтобы удалить по номеру):",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    await message.answer(
        "Подписка удалена." if await db.remove(message.from_user.id, url) else "Такой подписки не было.",
        reply_markup=MAIN_KEYBOARD,
    )


@router.message(RemoveStates.waiting_for_url, F.text)
async def remove_url_received(message: Message, db: Database, state: FSMContext) -> None:
    """Получаем URL от пользователя в рамках FSM-диалога /remove."""
    await state.clear()
    if not message.from_user:
        return
    url = extract_url(message.text)
    if not url:
        await message.answer(
            "Не удалось распознать ссылку. Отправьте корректную ссылку вида "
            "https://drova.io/stations/... или используйте /unsubscribe N.",
            reply_markup=MAIN_KEYBOARD,
        )
        return
    await message.answer(
        "Подписка удалена." if await db.remove(message.from_user.id, url) else "Такой подписки не было.",
        reply_markup=MAIN_KEYBOARD,
    )


@router.message(Command("clear"))
async def clear_command(message: Message, db: Database) -> None:
    if not message.from_user:
        return
    rows = await db.subscriptions_for_user(message.from_user.id)
    if not rows:
        await message.answer("Удалять нечего: подписок пока нет.", reply_markup=MAIN_KEYBOARD)
        return
    await message.answer(
        f"Удалить все подписки? Сейчас их {len(rows)}.",
        reply_markup=CLEAR_CONFIRM_KEYBOARD,
    )


@router.callback_query(F.data.startswith("clear:"))
async def clear_callback(callback: CallbackQuery, db: Database) -> None:
    if not callback.from_user or not callback.data:
        return
    action = callback.data.split(":", 1)[1]
    if action == "yes":
        count = await db.clear(callback.from_user.id)
        await callback.answer(f"Удалено подписок: {count}", show_alert=False)
        # Редактируем сообщение вместо удаления — пользователь видит подтверждение
        if callback.message:
            await callback.message.edit_text(f"✅ Все подписки удалены ({count} шт.).")
    else:
        await callback.answer("Отменено", show_alert=False)
        if callback.message:
            await delete_message_safely(callback.message)


@router.message(CommentStates.waiting_for_comment, F.text)
async def comment_text_received(message: Message, db: Database, state: FSMContext) -> None:
    """Получаем текст комментария от пользователя и сохраняем.

    Зарегистрирован ДО catch-all text_message — иначе aiogram 3 (first-match)
    поймает сообщение раньше FSM-фильтра и вернёт «пришлите ссылку».
    """
    if not message.from_user or not message.text:
        return

    comment = message.text.strip()

    if not comment:
        await message.answer("Комментарий не может быть пустым. Введите текст:")
        return  # FSM остаётся активным — ждём нового ввода

    if len(comment) > COMMENT_MAX_LEN:
        await message.answer(
            f"⚠️ Слишком длинный комментарий: {len(comment)} символов.\n"
            f"Максимум — {COMMENT_MAX_LEN}. Пожалуйста, сократите и пришлите снова:"
        )
        return  # FSM остаётся активным — ждём нового ввода

    data = await state.get_data()
    await state.clear()

    url: str | None = data.get("comment_url")
    if not url:
        await message.answer("Не удалось определить сервер. Попробуйте снова через /list.")
        return

    await db.set_comment(message.from_user.id, url, comment)
    await message.answer(
        f"✅ Комментарий сохранён:\n«{comment}»",
        reply_markup=MAIN_KEYBOARD,
    )

    # Обновляем все сообщения с клавиатурами комментариев (могут быть из нескольких окон)
    keyboard_refs: list[dict[str, int]] = data.get("keyboard_refs", [])
    if keyboard_refs and message.bot:
        keyboard = await build_comment_keyboard(db, message.from_user.id)
        for ref in keyboard_refs:
            try:
                await message.bot.edit_message_text(
                    "Комментарии к серверам:",
                    chat_id=ref["chat_id"],
                    message_id=ref["msg_id"],
                    reply_markup=keyboard,
                )
            except TelegramAPIError:
                pass  # сообщение могло быть удалено — не критично


@router.message(F.text)
async def text_message(message: Message, db: Database, drova: DrovaClient) -> None:
    if not message.from_user:
        return
    url = extract_url(message.text)
    if not url:
        await message.answer(
            "Пришлите корректную ссылку на станцию: https://drova.io/stations/...",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    cache_key = (message.from_user.id, url)
    now = time.monotonic()
    async with _subscribe_rate_lock:
        _evict_rate_cache(now)
        last_sent = _subscribe_rate_cache.get(cache_key, 0.0)
        if now - last_sent < SUBSCRIBE_RATE_LIMIT_SECONDS:
            remaining = int(SUBSCRIBE_RATE_LIMIT_SECONDS - (now - last_sent))
            await message.answer(
                f"Эта ссылка уже обрабатывается. Повторите через {remaining} с.",
                reply_markup=MAIN_KEYBOARD,
            )
            return
        _subscribe_rate_cache[cache_key] = now

    await message.bot.send_chat_action(chat_id=message.chat.id, action="typing")  # type: ignore[union-attr]
    try:
        snapshot = await drova.snapshot(url)
    except (httpx.RequestError, httpx.HTTPStatusError, ParserError, ValueError) as exc:
        LOG.warning("Subscribe failed for %s: %s", url, exc)
        # Сбрасываем rate-limit при ошибке, чтобы пользователь мог сразу повторить
        async with _subscribe_rate_lock:
            _subscribe_rate_cache.pop(cache_key, None)
        await message.answer(
            "Не удалось прочитать статус станции через Drova API. Проверьте ссылку или попробуйте позже.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    result = await db.add_subscription(message.from_user.id, url, snapshot.name, snapshot.status)
    if result == "limit":
        subs = await db.subscriptions_for_user(message.from_user.id)
        subs_list = "\n".join(f"  {i}. {row['server_name']}" for i, row in enumerate(subs, 1))
        await message.answer(
            f"Лимит подписок: {MAX_SUBSCRIPTIONS_PER_USER}. Ваши текущие подписки:\n"
            f"{subs_list}\n\n"
            "Удалите одну через /delete или /unsubscribe N, затем добавьте новую ссылку.",
            reply_markup=MAIN_KEYBOARD,
        )
        return

    action = "Подписка обновлена" if result == "updated" else "Подписка добавлена"
    if snapshot.status == "ready":
        if snapshot.published:
            status_msg = (
                f"{action}: {snapshot.name}.\n"
                f"Текущий статус: {status_text(snapshot.status)}.\n"
                "Если станция займётся и снова освободится, я пришлю уведомление."
            )
        else:
            status_msg = (
                f"{action}: {snapshot.name}.\n"
                f"Текущий статус: {status_text(snapshot.status)} (скрыт для пользователей).\n"
                "Станция свободна, но сейчас скрыта для обычных пользователей. "
                "Мерчанты получат уведомление при освобождении, "
                "пользователи — когда сервер станет публично доступен."
            )
    elif snapshot.status == "offline":
        status_msg = (
            f"{action}: {snapshot.name}.\n"
            f"Текущий статус: {status_text(snapshot.status)}.\n"
            "Сервер сейчас недоступен. Уведомление придёт, когда он вернётся к работе."
        )
    else:  # busy
        status_msg = (
            f"{action}: {snapshot.name}.\n"
            f"Текущий статус: {status_text(snapshot.status)}.\n"
            "Уведомление придёт при освобождении. Если станция снова займётся и "
            "освободится позже, уведомление придёт снова."
        )
    await message.answer(status_msg, reply_markup=MAIN_KEYBOARD)

    # Предложить добавить комментарий (получаем ID только что добавленной/обновлённой подписки)
    subs = await db.subscriptions_for_user(message.from_user.id)
    sub_row = next((r for r in subs if r["url"] == url), None)
    if sub_row:
        existing_comment = await db.get_comment(message.from_user.id, url)
        if not existing_comment:
            await message.answer(
                "Хотите добавить комментарий к этому серверу?\n"
                "Он будет виден в /list и в уведомлениях.",
                reply_markup=comment_prompt_keyboard(sub_row["id"]),
            )


# ---------------------------------------------------------------------------
# Comment handlers
# ---------------------------------------------------------------------------

@router.callback_query(F.data.startswith("comment:add:") | F.data.startswith("comment:edit:"))
async def comment_add_or_edit_callback(callback: CallbackQuery, db: Database, state: FSMContext) -> None:
    if not callback.from_user or not callback.data:
        return
    try:
        subscription_id = int(callback.data.split(":", 2)[2])
    except (IndexError, ValueError):
        await callback.answer("Некорректная кнопка", show_alert=True)
        return

    url = await db.get_url_by_subscription_id(callback.from_user.id, subscription_id)
    if not url:
        await callback.answer("Подписка не найдена — возможно, она уже удалена.", show_alert=True)
        return

    existing = await db.get_comment(callback.from_user.id, url)

    # Накапливаем все сообщения с клавиатурами, которые нужно обновить после сохранения.
    # Пользователь может открыть редактирование из нескольких окон — обновим все.
    data = await state.get_data()
    keyboard_refs: list[dict[str, int]] = data.get("keyboard_refs", [])
    if callback.message:
        keyboard_refs.append({
            "msg_id":  callback.message.message_id,
            "chat_id": callback.message.chat.id,
        })

    await state.set_state(CommentStates.waiting_for_comment)
    await state.update_data(
        comment_url=url,
        comment_sub_id=subscription_id,
        keyboard_refs=keyboard_refs,
    )
    await callback.answer()
    prompt = (
        f"Текущий комментарий: «{existing}»\n\nВведите новый текст (до 500 символов):"
        if existing else
        "Введите комментарий к серверу (до 500 символов):"
    )
    if callback.message:
        await callback.message.answer(prompt)


@router.callback_query(F.data.startswith("comment:later:"))
async def comment_later_callback(callback: CallbackQuery) -> None:
    await callback.answer()
    if callback.message:
        await callback.message.edit_text("Хорошо, можно добавить комментарий позже через /list.")


@router.callback_query(F.data.startswith("comment:del:"))
async def comment_delete_callback(callback: CallbackQuery, db: Database) -> None:
    """Запрашивает подтверждение перед удалением комментария."""
    if not callback.from_user or not callback.data:
        return
    try:
        subscription_id = int(callback.data.split(":", 2)[2])
    except (IndexError, ValueError):
        await callback.answer("Некорректная кнопка", show_alert=True)
        return

    url = await db.get_url_by_subscription_id(callback.from_user.id, subscription_id)
    if not url:
        await callback.answer("Подписка не найдена.", show_alert=True)
        return

    confirm_keyboard = InlineKeyboardMarkup(inline_keyboard=[[
        InlineKeyboardButton(text="✅ Да, удалить", callback_data=f"comment:del_yes:{subscription_id}"),
        InlineKeyboardButton(text="❌ Отмена",      callback_data=f"comment:del_no:{subscription_id}"),
    ]])
    await callback.answer()
    if callback.message:
        await callback.message.edit_text(
            "Удалить комментарий к этому серверу?",
            reply_markup=confirm_keyboard,
        )


@router.callback_query(F.data.startswith("comment:del_yes:"))
async def comment_delete_confirm_callback(callback: CallbackQuery, db: Database) -> None:
    """Удаляет комментарий после подтверждения и возвращает обновлённую клавиатуру."""
    if not callback.from_user or not callback.data:
        return
    try:
        subscription_id = int(callback.data.split(":", 2)[2])
    except (IndexError, ValueError):
        await callback.answer("Некорректная кнопка", show_alert=True)
        return

    url = await db.get_url_by_subscription_id(callback.from_user.id, subscription_id)
    if not url:
        await callback.answer("Подписка не найдена.", show_alert=True)
        return

    await db.delete_comment(callback.from_user.id, url)
    await callback.answer("🗑 Комментарий удалён.")

    if callback.message:
        keyboard = await build_comment_keyboard(db, callback.from_user.id)
        if keyboard:
            await callback.message.edit_text("Комментарии к серверам:", reply_markup=keyboard)
        else:
            await callback.message.edit_text("Комментариев нет.")


@router.callback_query(F.data.startswith("comment:del_no:"))
async def comment_delete_cancel_callback(callback: CallbackQuery, db: Database) -> None:
    """Отменяет удаление комментария и возвращает клавиатуру управления."""
    if not callback.from_user or not callback.data:
        return
    await callback.answer("Отменено.")
    if callback.message:
        keyboard = await build_comment_keyboard(db, callback.from_user.id)
        if keyboard:
            await callback.message.edit_text("Комментарии к серверам:", reply_markup=keyboard)
        else:
            await callback.message.edit_text("Комментариев нет.")


@router.callback_query(F.data.startswith("notify_refresh:"))
async def notify_refresh_callback(
    callback: CallbackQuery, db: Database, monitor: "Monitor"
) -> None:
    """Обновить уведомление об освобождении сервера без запроса к API.

    Данные берутся из Monitor._last_statuses (кэш последнего цикла мониторинга).
    Если кэш пуст — читаем last_status из БД как запасной вариант.
    """
    if not callback.from_user or not callback.message or not callback.data:
        return
    remaining = _check_refresh_cooldown(callback.from_user.id)
    if remaining is not None:
        await callback.answer(f"Подождите {remaining:.0f} сек")
        return

    uuid = callback.data.split(":", 1)[1]
    if not STATION_UUID_RE.fullmatch(uuid):
        await callback.answer("Некорректный идентификатор станции", show_alert=True)
        return

    url = f"https://drova.io/stations/{uuid}"

    # Пробуем взять статус из кэша монитора
    snap = monitor._last_statuses.get(url)
    if snap is not None:
        current_status = snap.status
    else:
        # Fallback: берём из БД
        rows = await db.connection.execute_fetchall(
            "SELECT last_status FROM monitoring WHERE url = ? LIMIT 1", (url,)
        )
        if rows:
            current_status = rows[0]["last_status"]
        else:
            await callback.answer("Данные ещё не загружены, попробуйте позже", show_alert=True)
            return

    if current_status == "ready":
        await callback.answer("Сервер всё ещё свободен ✅")
        return

    # Сервер занят или офлайн — дополняем исходное сообщение и меняем значок
    original_text = callback.message.text or ""
    updated_text = original_text.replace("🟢", "🔴", 1).replace("🔒", "🔴", 1)
    if current_status == "busy":
        updated_text += "\n\n🔴 Сервер уже занят."
    else:
        updated_text += "\n\n🔴 Сервер недоступен (офлайн)."

    try:
        await callback.message.edit_text(updated_text, reply_markup=None)
    except TelegramAPIError as exc:
        LOG.warning("notify_refresh: cannot edit message: %s", exc)
    await callback.answer("Обновлено")


def pluralize_users(n: int) -> str:
    """Правильное склонение слова «пользователь» для числа n."""
    if 11 <= n % 100 <= 19:
        return f"{n} пользователей"
    rem = n % 10
    if rem == 1:
        return f"{n} пользователь"
    if 2 <= rem <= 4:
        return f"{n} пользователя"
    return f"{n} пользователей"


def notify_refresh_keyboard(url: str) -> InlineKeyboardMarkup:
    """Клавиатура с кнопкой «Обновить» для уведомлений об освобождении сервера.

    В callback_data передаём только UUID станции (36 символов), что даёт итоговую
    строку «notify_refresh:<uuid>» = 51 байт — хорошо укладывается в лимит 64 байт.
    """
    uuid = station_uuid_from_url(url)  # уже валидирован при подписке, ошибок быть не должно
    return InlineKeyboardMarkup(
        inline_keyboard=[[
            InlineKeyboardButton(text="Обновить", callback_data=f"notify_refresh:{uuid}")
        ]]
    )


def current_weekly_donate_due(now: datetime | None = None) -> datetime | None:
    """Вернуть UTC-время понедельника 19:00 МСК текущей недели, если оно уже наступило."""
    now_msk = (now or datetime.now(timezone.utc)).astimezone(MOSCOW_TZ)
    monday = (now_msk - timedelta(days=now_msk.weekday())).replace(
        hour=19,
        minute=0,
        second=0,
        microsecond=0,
    )
    if now_msk < monday:
        return None
    return monday.astimezone(timezone.utc)


def _format_ready_notification(
    name: str,
    url: str,
    prev_status: str,
    comment: str | None,
    others_count: int,
    published: bool,
) -> str:
    """Сформировать текст уведомления об освобождении сервера.

    Единая точка формирования: применяется и для обычных пользователей (published=True),
    и для мерчантов (published=False), исключая дублирование логики в poll_url.

    ``prev_status`` определяет заголовок: офлайн→готов отличается от занят→готов.
    ``others_count`` — число других уведомлённых пользователей (не включая текущего).
    """
    comment_line = f"\n💬 {comment}" if comment else ""
    others_line = (
        f"\nОб этом также уведомлены {pluralize_users(others_count)}."
        if others_count > 0 else ""
    )
    if published:
        header = (
            "🟢 Сервер вернулся к работе!" if prev_status == "offline"
            else "🟢 Станция свободна!"
        )
        status_line = f"Статус: {status_text('ready')}"
    else:
        header = (
            "🔒 Сервер вернулся к работе и доступен мерчанту!" if prev_status == "offline"
            else "🔒 Станция освободилась и доступна мерчанту!"
        )
        status_line = "Статус: свободен, но скрыт для обычных пользователей"
    return f"{header}\n{name}\n{status_line}\n{url}{comment_line}{others_line}"


# ---------------------------------------------------------------------------
# Monitor
# ---------------------------------------------------------------------------

class Monitor:
    POLL_CONCURRENCY = 5  # максимальное число параллельных HTTP-запросов к Drova

    def __init__(
        self,
        db: Database,
        drova: DrovaClient,
        bot: Bot,
        delay: float,
        admin_ids: tuple[int, ...] = (),
    ) -> None:
        self.db = db
        self.drova = drova
        self.bot = bot
        self.delay = delay
        self.admin_ids = admin_ids
        self.lock = asyncio.Lock()
        self._semaphore = asyncio.Semaphore(self.POLL_CONCURRENCY)
        # url -> (deadline_monotonic, {user_id: (message_id, text)})
        # Хранит уведомления об освобождении, которые нужно дополнить «Уже заняли»,
        # если сервер снова займут в течение READY_WATCH_SECONDS.
        self._ready_watch: dict[str, tuple[float, dict[int, tuple[int, str]]]] = {}
        # Глобальный кэш последних известных статусов: url -> Snapshot.
        # Заполняется в poll_url при каждом успешном опросе API.
        # Служит «единым источником правды» для всех callback-обработчиков,
        # позволяя им не делать прямых запросов к API.
        self._last_statuses: dict[str, Snapshot] = {}
        # Admin alert state: consecutive poll failures per URL, URLs already reported as down,
        # and changes waiting for the next (rate-limited) digest.
        self._fail_streaks: dict[str, int] = {}
        self._alerted_urls: set[str] = set()
        self._pending_down: dict[str, str] = {}
        self._pending_up: set[str] = set()
        self._last_admin_alert: float | None = None

    async def load_state(self) -> None:
        """Загрузить персистентное состояние из БД при старте бота.

        Вызывается один раз из run_bot() после Database.init().
        Восстанавливает _ready_watch, чтобы после перезагрузки бот не терял
        5-минутные окна наблюдения.
        """
        self._ready_watch = await self.db.load_ready_watch()
        LOG.info("Monitor: restored %d ready-watch entries from DB", len(self._ready_watch))

    async def weekly_donate_broadcast(self) -> None:
        """Поставить в очередь еженедельное напоминание о /donate.

        Если бот был выключен в понедельник в 19:00 МСК, при следующем запуске
        будет поставлена пропущенная рассылка за текущую неделю.
        """
        due_at = current_weekly_donate_due()
        if due_at is None:
            return
        if await self.db.campaign_exists(DONATE_CAMPAIGN_KIND, due_at):
            return
        user_ids = await self.db.all_unmuted_user_ids()
        queued = await self.db.enqueue_many_notifications(
            user_ids,
            DONATE_CAMPAIGN_KIND,
            WEEKLY_DONATE_TEXT,
            parse_mode="MarkdownV2",
            not_before=due_at,
        )
        await self.db.create_campaign(DONATE_CAMPAIGN_KIND, due_at)
        LOG.info(
            "Weekly donate broadcast: queued %d users for due_at=%s",
            queued, due_at.isoformat(),
        )
        await self.process_notification_queue()

    async def _enqueue_admin_alert(self, text: str) -> None:
        for admin_id in self.admin_ids:
            await self.db.enqueue_notification(
                admin_id,
                ADMIN_ERROR_KIND,
                text,
                bypass_mute=True,
            )
        await self.process_notification_queue()

    def _on_poll_failure(self, url: str, error: str) -> None:
        streak = self._fail_streaks.get(url, 0) + 1
        self._fail_streaks[url] = streak
        if streak >= ADMIN_ALERT_FAIL_STREAK and url not in self._alerted_urls:
            self._alerted_urls.add(url)
            self._pending_down[url] = error
            self._pending_up.discard(url)

    def _on_poll_success(self, url: str) -> None:
        self._fail_streaks.pop(url, None)
        if url in self._alerted_urls:
            self._alerted_urls.discard(url)
            # a failure that recovered before the digest went out is not worth reporting at all
            if self._pending_down.pop(url, None) is None:
                self._pending_up.add(url)

    def _station_label(self, url: str) -> str:
        snapshot = self._last_statuses.get(url)
        return f"{snapshot.name} ({url})" if snapshot else url

    async def flush_admin_alerts(self) -> None:
        """Send one digest with stations that went down or came back, at most once per interval."""
        if not self.admin_ids or not (self._pending_down or self._pending_up):
            return
        now = time.monotonic()
        if (
            self._last_admin_alert is not None
            and now - self._last_admin_alert < ADMIN_ALERT_MIN_INTERVAL_SECONDS
        ):
            return
        lines = ["Drova: опрос станций", ""]
        if self._pending_down:
            lines.append(f"Не отвечают {ADMIN_ALERT_FAIL_STREAK}+ опроса подряд:")
            lines += [f"• {self._station_label(u)}\n  {e[:300]}" for u, e in self._pending_down.items()]
        if self._pending_up:
            if self._pending_down:
                lines.append("")
            lines.append("Снова отвечают:")
            lines += [f"• {self._station_label(u)}" for u in sorted(self._pending_up)]
        lines += ["", f"Время UTC: {datetime.now(timezone.utc).isoformat(timespec='seconds')}"]
        self._pending_down.clear()
        self._pending_up.clear()
        self._last_admin_alert = now
        await self._enqueue_admin_alert("\n".join(lines)[:4000])

    async def process_notification_queue(self) -> None:
        dropped = await self.db.drop_stale_notifications(ADMIN_ERROR_KIND, ADMIN_ERROR_MAX_AGE_MINUTES)
        if dropped:
            LOG.info("Notification queue: dropped %d stale admin alert(s)", dropped)
        rows = await self.db.due_notifications()
        if not rows:
            return
        LOG.info("Notification queue: processing %d due notification(s)", len(rows))
        for row in rows:
            try:
                await self.bot.send_message(
                    chat_id=row["user_id"],
                    text=row["text"],
                    parse_mode=row["parse_mode"],
                )
                await self.db.mark_notification_sent(row["id"])
            except TelegramForbiddenError:
                LOG.warning("Notification queue: user %s blocked the bot", row["user_id"])
                if row["kind"] != ADMIN_ERROR_KIND:
                    await self.db.clear(row["user_id"])
                await self.db.mark_notification_sent(row["id"])
            except TelegramNetworkError as exc:
                # Telegram is unreachable: every other row would fail the same way, retry on the next run
                LOG.warning("Notification queue: Telegram unreachable, postponing: %s", exc)
                await self.db.mark_notification_failed(row["id"], str(exc))
                break
            except TelegramAPIError as exc:
                LOG.warning("Notification queue: cannot message user %s: %s", row["user_id"], exc)
                await self.db.mark_notification_failed(row["id"], str(exc))
            await asyncio.sleep(0.05)  # мягкий rate-limit рассылки

    async def process_expired_mutes(self) -> None:
        user_ids = await self.db.consume_expired_mutes()
        if not user_ids:
            return
        LOG.info("Expired silence: sending status report to %d user(s)", len(user_ids))
        for user_id in user_ids:
            try:
                text = await build_unsilenced_status_text(self.db, self, user_id)
                await self.bot.send_message(
                    chat_id=user_id,
                    text=text,
                    reply_markup=STATUS_REFRESH_KEYBOARD,
                )
            except TelegramForbiddenError:
                LOG.warning("Expired silence: user %s blocked the bot", user_id)
                await self.db.clear(user_id)
            except TelegramAPIError as exc:
                LOG.warning("Expired silence: cannot message user %s: %s", user_id, exc)
            await asyncio.sleep(0.05)

    async def poll_once(self) -> None:
        if self.lock.locked():
            LOG.info("Previous polling cycle is still running; skipping this tick")
            return
        async with self.lock:
            urls = await self.db.urls()
            LOG.info(
                "Polling %d Drova station URL(s) (concurrency=%d)",
                len(urls), self.POLL_CONCURRENCY,
            )
            tasks = [asyncio.create_task(self._poll_url_guarded(url)) for url in urls]
            results = await asyncio.gather(*tasks, return_exceptions=True)
            for url, result in zip(urls, results):
                if isinstance(result, BaseException) and not isinstance(result, asyncio.CancelledError):
                    LOG.error("Unhandled exception leaked from task for %s: %r", url, result)
                    self._on_poll_failure(url, f"Unhandled task exception: {result!r}")
            await self.flush_admin_alerts()

    async def _poll_url_guarded(self, url: str) -> None:
        """Обёртка для параллельного запуска poll_url с ограничением конкурентности."""
        async with self._semaphore:
            try:
                await self.poll_url(url)
            except (httpx.RequestError, httpx.HTTPStatusError, ParserError, ValueError) as exc:
                error = describe_error(exc)
                LOG.warning("Failed to poll %s: %s", url, error)
                await self.db.record_error(url, error)
                self._on_poll_failure(url, error)
            except Exception as exc:
                LOG.exception("Unexpected polling error for %s", url)
                await self.db.record_error(url, describe_error(exc))
                self._on_poll_failure(url, f"Непредвиденная ошибка: {describe_error(exc)}")
            else:
                self._on_poll_success(url)
            finally:
                await asyncio.sleep(self.delay)

    async def poll_url(self, url: str) -> None:
        # Опрос идёт мимо кеша — монитор работает с актуальными данными
        # и одновременно обновляет кеш для последующих пользовательских запросов.
        snapshot = await self.drova.snapshot(url, use_cache=False)

        # Обновляем глобальный кэш статусов — единый источник правды для кнопок «Обновить».
        self._last_statuses[url] = snapshot

        await self.db.record_status(url, snapshot)

        # Обновляем имя сервера для всех подписчиков при каждом успешном опросе,
        # чтобы переименования отражались независимо от статуса и роли пользователя.
        await self.db.update_server_name(url, snapshot.name)

        rows = await self.db.subscriptions_for_url(url)
        notify_rows = [row for row in rows if not row["muted"]]
        LOG.info(
            "Polled %s: web=%s raw=%s published=%s pending=%d muted=%d notify=%d",
            snapshot.name, snapshot.status, snapshot.raw_status, snapshot.published,
            len(rows), len(rows) - len(notify_rows), len(notify_rows),
        )

        if snapshot.status == "busy":
            await self.db.mark_busy(url, snapshot.name)
            # Если сервер занят в течение 5-минутного окна наблюдения —
            # редактируем ранее отправленные уведомления об освобождении.
            await self._fire_taken_notifications(url)
            return

        if snapshot.status == "offline":
            await self.db.mark_offline(url, snapshot.name)
            await self._fire_taken_notifications(url)
            return

        # snapshot.status == "ready"
        # Если сервер скрыт (published=False), обычные пользователи не должны получать
        # уведомления — для них это эквивалентно offline. Мерчанты же получают
        # специальное уведомление: сервер свободен, но скрыт от обычных пользователей.

        # Загружаем все комментарии для данного URL одним запросом вместо N get_comment
        url_comments: dict[int, str] = await self.db.comments_for_url(url)

        # Разделяем подписчиков по роли (merchant_rows нужен для ветки published=False)
        merchant_rows = [row for row in notify_rows if row["is_merchant"]]

        if snapshot.published:
            # Сервер свободен и публично доступен — уведомляем всех
            rows_to_notify = notify_rows
            notified_ids: list[int] = []
            prev_statuses: dict[int, str] = {row["user_id"]: row["last_status"] for row in rows_to_notify}
            await self.db.mark_ready(url, snapshot.name)

            total_to_notify = len(rows_to_notify)
            # Словарь user_id -> (message_id, text) для 5-минутного наблюдения
            watch_messages: dict[int, tuple[int, str]] = {}

            for row in rows_to_notify:
                user_id = row["user_id"]
                text = _format_ready_notification(
                    name=snapshot.name,
                    url=url,
                    prev_status=prev_statuses.get(user_id, "busy"),
                    comment=url_comments.get(user_id),
                    others_count=total_to_notify - 1,
                    published=True,
                )
                try:
                    sent = await self.bot.send_message(
                        chat_id=user_id,
                        text=text,
                        reply_markup=notify_refresh_keyboard(url),
                    )
                    notified_ids.append(user_id)
                    watch_messages[user_id] = (sent.message_id, text)
                    LOG.info("Ready notification sent to user %s for %s", user_id, url)
                except TelegramForbiddenError:
                    LOG.warning("User %s blocked the bot; removing all their subscriptions", user_id)
                    await self.db.clear(user_id)
                except TelegramAPIError as exc:
                    LOG.warning("Cannot notify user %s about %s: %s", user_id, url, exc)

            await self.db.bulk_mark_notified(notified_ids, url)

            # Регистрируем окно наблюдения, чтобы поймать быстрое занятие
            if watch_messages:
                deadline = time.monotonic() + READY_WATCH_SECONDS
                self._ready_watch[url] = (deadline, watch_messages)
                # Персистируем в БД — при перезагрузке бот не потеряет окно наблюдения
                expires_at = datetime.now(timezone.utc) + timedelta(seconds=READY_WATCH_SECONDS)
                await self.db.save_ready_watch(url, expires_at, watch_messages)
                LOG.debug("Ready-watch registered for %s (%d users)", url, len(watch_messages))
        else:
            # Сервер свободен, но скрыт (published=False)
            # — уведомляем только мерчантов, обычных пользователей не трогаем

            merchant_notified: list[int] = []
            prev_statuses_m: dict[int, str] = {row["user_id"]: row["last_status"] for row in merchant_rows}
            total_merchants = len(merchant_rows)

            for row in merchant_rows:
                user_id = row["user_id"]
                text = _format_ready_notification(
                    name=snapshot.name,
                    url=url,
                    prev_status=prev_statuses_m.get(user_id, "busy"),
                    comment=url_comments.get(user_id),
                    others_count=total_merchants - 1,
                    published=False,
                )
                try:
                    await self.bot.send_message(
                        chat_id=user_id,
                        text=text,
                        reply_markup=notify_refresh_keyboard(url),
                    )
                    merchant_notified.append(user_id)
                    LOG.info(
                        "Merchant-only notification sent to user %s for %s (published=False)",
                        user_id, url,
                    )
                except TelegramForbiddenError:
                    LOG.warning("User %s blocked the bot; removing all their subscriptions", user_id)
                    await self.db.clear(user_id)
                except TelegramAPIError as exc:
                    LOG.warning("Cannot notify merchant %s about %s: %s", user_id, url, exc)

            # Обновляем статус только для мерчантов — они уже уведомлены
            if merchant_notified:
                await self.db.mark_ready_for_user_ids(url, snapshot.name, merchant_notified)

            # Обычные пользователи остаются в состоянии ожидания (pending_notify=1,
            # last_status не меняется), пока сервер не станет published=True

    async def _fire_taken_notifications(self, url: str) -> None:
        """Если URL в окне наблюдения — редактировать уведомления об освобождении,
        добавив «Уже заняли», сменить 🟢 на 🔴 и убрать кнопку «Обновить»."""
        entry = self._ready_watch.pop(url, None)
        # Всегда удаляем из БД — даже если окно истекло, запись уже не нужна
        await self.db.delete_ready_watch(url)
        if entry is None:
            return
        deadline, watch_messages = entry
        if time.monotonic() > deadline:
            return  # окно наблюдения уже истекло
        LOG.info("Server %s taken within watch window; editing %d notifications", url, len(watch_messages))
        for user_id, (message_id, original_text) in watch_messages.items():
            updated_text = original_text.replace("🟢", "🔴", 1)
            updated_text += "\n\n🔴 Уже заняли."
            try:
                await self.bot.edit_message_text(
                    chat_id=user_id,
                    message_id=message_id,
                    text=updated_text,
                    reply_markup=None,
                )
                LOG.debug("Taken edit sent to user %s for %s", user_id, url)
            except TelegramForbiddenError:
                LOG.warning("Taken notify: user %s blocked the bot", user_id)
                await self.db.clear(user_id)
            except TelegramAPIError as exc:
                LOG.warning("Taken notify: cannot edit message for user %s: %s", user_id, exc)


# ---------------------------------------------------------------------------
# Entry points
# ---------------------------------------------------------------------------

async def run_bot() -> None:
    settings = load_settings()
    logging.basicConfig(
        level=settings.log_level,
        format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
    )
    db = Database(settings.database_path)
    await db.connect()
    await db.init()
    drova = DrovaClient(
        settings.http_timeout_seconds,
        cache_ttl_seconds=settings.cache_ttl_seconds,
    )
    bot = Bot(settings.bot_token)
    await register_bot_commands(bot, settings)
    dispatcher = Dispatcher()
    dispatcher.include_router(router)
    monitor = Monitor(db, drova, bot, settings.request_delay_seconds, settings.admin_ids)
    await monitor.load_state()
    scheduler = AsyncIOScheduler(timezone="UTC")
    scheduler.add_job(
        monitor.poll_once,
        "interval",
        seconds=settings.poll_interval_seconds,
        id="drova_monitor_poll",
        max_instances=1,
        coalesce=True,
        next_run_time=datetime.now(timezone.utc) + timedelta(seconds=5),
    )
    scheduler.add_job(
        monitor.process_notification_queue,
        "interval",
        seconds=30,
        id="drova_notification_queue",
        max_instances=1,
        coalesce=True,
        next_run_time=datetime.now(timezone.utc) + timedelta(seconds=10),
    )
    scheduler.add_job(
        monitor.process_expired_mutes,
        "interval",
        seconds=60,
        id="drova_expired_mutes",
        max_instances=1,
        coalesce=True,
        next_run_time=datetime.now(timezone.utc) + timedelta(seconds=20),
    )
    # Проверяем каждую минуту: после понедельника 19:00 МСК ставим рассылку в очередь.
    # Если бот был выключен в нужный момент, при следующем запуске неделя догонится.
    scheduler.add_job(
        monitor.weekly_donate_broadcast,
        "interval",
        seconds=60,
        id="drova_weekly_donate",
        max_instances=1,
        coalesce=True,
        next_run_time=datetime.now(timezone.utc) + timedelta(seconds=15),
    )
    scheduler.start()
    dispatcher["db"] = db
    dispatcher["drova"] = drova
    dispatcher["settings"] = settings
    dispatcher["monitor"] = monitor
    LOG.info("Bot started; poll interval is %s seconds", settings.poll_interval_seconds)
    try:
        await dispatcher.start_polling(bot, allowed_updates=dispatcher.resolve_used_update_types())
    finally:
        LOG.info("Shutting down scheduler (waiting for running jobs)…")
        scheduler.shutdown(wait=True)
        await drova.close()
        await bot.session.close()
        await db.close()
        LOG.info("Shutdown complete")


async def probe() -> None:
    settings = load_settings()
    db = Database(settings.database_path)
    await db.connect()
    await db.init()
    drova = DrovaClient(settings.http_timeout_seconds, cache_ttl_seconds=settings.cache_ttl_seconds)
    try:
        rows = await db.connection.execute_fetchall(
            """
            SELECT url, server_name, last_status, pending_notify
            FROM monitoring
            ORDER BY server_name
            """
        )
        for row in rows:
            snapshot = await drova.snapshot(row["url"])
            print(f"{snapshot.name}: web={snapshot.status} raw={snapshot.raw_status}")
            print(f"db={row['last_status']} pending_notify={row['pending_notify']} url={row['url']}")
    finally:
        await drova.close()
        await db.close()


async def check_token() -> None:
    settings = load_settings()
    bot = Bot(settings.bot_token)
    try:
        me = await bot.get_me()
        print(f"bot_id={me.id}")
        print(f"username=@{me.username}")
        print(f"name={me.full_name}")
    finally:
        await bot.session.close()


async def setup_commands() -> None:
    settings = load_settings()
    bot = Bot(settings.bot_token)
    try:
        await register_bot_commands(bot, settings)
        print("commands=updated")
    finally:
        await bot.session.close()


async def clear_db() -> None:
    settings = load_settings()
    db = Database(settings.database_path)
    await db.connect()
    await db.init()
    try:
        await db.connection.execute("DELETE FROM monitoring")
        await db.connection.execute("DELETE FROM sqlite_sequence WHERE name = 'monitoring'")
        await db.connection.commit()
        count = (
            await db.connection.execute_fetchall("SELECT COUNT(*) AS count FROM monitoring")
        )[0]["count"]
        print(f"subscriptions={count}")
    finally:
        await db.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="Drova Telegram monitor bot")
    parser.add_argument(
        "command",
        nargs="?",
        choices=["run", "probe", "check-token", "setup-commands", "clear-db"],
        default="run",
    )
    args = parser.parse_args()
    try:
        if args.command == "probe":
            asyncio.run(probe())
        elif args.command == "check-token":
            asyncio.run(check_token())
        elif args.command == "setup-commands":
            asyncio.run(setup_commands())
        elif args.command == "clear-db":
            asyncio.run(clear_db())
        else:
            asyncio.run(run_bot())
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        sys.exit(0)


if __name__ == "__main__":
    main()
