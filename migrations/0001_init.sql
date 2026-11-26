CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    is_merchant INTEGER NOT NULL DEFAULT 0,
    role_selected INTEGER NOT NULL DEFAULT 0,
    muted INTEGER NOT NULL DEFAULT 0,
    mute_until INTEGER,
    tz TEXT NOT NULL DEFAULT 'Europe/Moscow',
    quiet_enabled INTEGER NOT NULL DEFAULT 0,
    quiet_start INTEGER NOT NULL DEFAULT 1380,
    quiet_end INTEGER NOT NULL DEFAULT 480,
    notify_offline_return INTEGER NOT NULL DEFAULT 1,
    show_game INTEGER NOT NULL DEFAULT 1,
    blocked_at INTEGER,
    created_at INTEGER NOT NULL,
    seen_at INTEGER NOT NULL
);

CREATE TABLE stations (
    uuid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('ready', 'busy', 'offline')),
    raw_state TEXT,
    published INTEGER NOT NULL DEFAULT 1,
    city TEXT,
    product_id TEXT,
    games_count INTEGER,
    status_since INTEGER NOT NULL,
    checked_at INTEGER
);

CREATE TABLE subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    station TEXT NOT NULL REFERENCES stations (uuid),
    last_status TEXT NOT NULL CHECK (last_status IN ('ready', 'busy', 'offline')),
    pending INTEGER NOT NULL DEFAULT 1,
    notify_busy INTEGER NOT NULL DEFAULT 0,
    notify_offline INTEGER NOT NULL DEFAULT 0,
    comment TEXT,
    comment_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (user_id, station)
);
CREATE INDEX ix_subscriptions_station ON subscriptions (station);

CREATE TABLE transitions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    station TEXT NOT NULL,
    from_status TEXT CHECK (from_status IS NULL OR from_status IN ('ready', 'busy', 'offline')),
    to_status TEXT NOT NULL CHECK (to_status IN ('ready', 'busy', 'offline')),
    product_id TEXT,
    at INTEGER NOT NULL
);
CREATE INDEX ix_transitions_station_at ON transitions (station, at);

CREATE TABLE products (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);

CREATE TABLE queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    station TEXT,
    text TEXT NOT NULL,
    keyboard TEXT,
    not_before INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'sent', 'dropped')),
    message_id INTEGER,
    taken_edited INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    done_at INTEGER
);
CREATE INDEX ix_queue_due ON queue (state, not_before);
CREATE INDEX ix_queue_station ON queue (station, kind);
CREATE INDEX ix_queue_message ON queue (chat_id, message_id);

CREATE TABLE campaigns (
    kind TEXT NOT NULL,
    due_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (kind, due_at)
);

CREATE TABLE poll_errors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    station TEXT NOT NULL,
    error TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX ix_poll_errors_at ON poll_errors (at);

-- Read-only views with the column names of the old schema, used by external tools.
CREATE VIEW monitoring AS
SELECT
    s.id,
    s.user_id,
    'https://drova.io/stations/' || s.station AS url,
    st.name AS server_name,
    s.last_status,
    s.pending AS pending_notify
FROM subscriptions AS s
JOIN stations AS st ON st.uuid = s.station;

CREATE VIEW user_settings AS
SELECT
    id AS user_id,
    muted,
    CASE WHEN mute_until IS NULL THEN NULL
         ELSE strftime('%Y-%m-%dT%H:%M:%S+00:00', mute_until, 'unixepoch') END AS mute_until,
    is_merchant,
    role_selected
FROM users;

CREATE VIEW server_comments AS
SELECT
    s.id,
    s.user_id,
    'https://drova.io/stations/' || s.station AS url,
    s.comment,
    datetime(COALESCE(s.comment_at, s.created_at), 'unixepoch') AS updated_at
FROM subscriptions AS s
WHERE s.comment IS NOT NULL;
