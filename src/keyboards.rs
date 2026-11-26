use crate::db::{SubRow, UserRow};
use crate::telegram::{Button, Inline, Markup};
use crate::texts::{self, BTN_HELP, BTN_LIST, BTN_SETTINGS, BTN_STATUS, PLACEHOLDER};
use crate::tz;
use crate::util::{clip, station_url};

pub fn main() -> Markup {
    Markup::Reply(
        vec![
            vec![BTN_STATUS.to_owned(), BTN_LIST.to_owned()],
            vec![BTN_SETTINGS.to_owned(), BTN_HELP.to_owned()],
        ],
        PLACEHOLDER.to_owned(),
    )
}

fn name(s: &SubRow) -> String {
    clip(&s.station.name, 40)
}

pub fn list(subs: &[SubRow]) -> Inline {
    let mut rows: Inline = subs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            vec![Button::cb(
                format!("{}. {}", i + 1, name(s)),
                format!("c:{}", s.id),
            )]
        })
        .collect();
    rows.push(vec![Button::cb("🔄 Обновить", "l:r")]);
    rows
}

pub fn card(s: &SubRow, merchant: bool) -> Inline {
    let mut rows = vec![
        vec![
            Button::cb("📜 История", format!("h:{}", s.id)),
            Button::cb("💬 Комментарий", format!("cm:a:{}", s.id)),
        ],
        vec![
            Button::cb("🔗 Поделиться", format!("sh:{}", s.id)),
            Button::cb("🗑 Удалить", format!("dq:{}", s.id)),
        ],
    ];
    if merchant {
        rows.push(vec![
            Button::cb(
                format!("▶️ Начало сессии: {}", texts::on_off(s.notify_busy)),
                format!("mf:b:{}", s.id),
            ),
            Button::cb(
                format!("📴 Офлайн: {}", texts::on_off(s.notify_offline)),
                format!("mf:o:{}", s.id),
            ),
        ]);
    }
    rows.push(vec![Button::url(
        "🌐 Открыть на drova.io",
        station_url(&s.station.uuid),
    )]);
    rows.push(vec![Button::cb("« К списку", "l:b")]);
    rows
}

pub fn back_to_card(id: i64) -> Inline {
    vec![vec![Button::cb("« К станции", format!("c:{id}"))]]
}

pub fn delete_confirm(id: i64) -> Inline {
    vec![vec![
        Button::cb("Да, удалить", format!("dy:{id}")),
        Button::cb("Отмена", format!("c:{id}")),
    ]]
}

pub fn status() -> Inline {
    vec![vec![Button::cb("🔄 Обновить", "st:r")]]
}

pub fn notify(uuid: &str) -> Inline {
    vec![vec![
        Button::cb("🔄 Обновить", format!("nr:{uuid}")),
        Button::url("🌐 Открыть", station_url(uuid)),
    ]]
}

pub fn open_station(uuid: &str) -> Inline {
    vec![vec![Button::url(
        "🌐 Открыть на drova.io",
        station_url(uuid),
    )]]
}

pub fn pick(subs: &[SubRow], prefix: &str) -> Inline {
    subs.iter()
        .map(|s| vec![Button::cb(name(s), format!("{prefix}:{}", s.id))])
        .collect()
}

pub fn comments(subs: &[SubRow]) -> Inline {
    subs.iter()
        .map(|s| {
            let has = s.comment.is_some();
            let mut row = vec![Button::cb(
                format!(
                    "{} {}",
                    if has { "✏️" } else { "➕" },
                    clip(&s.station.name, 30)
                ),
                format!("cm:a:{}", s.id),
            )];
            if has {
                row.push(Button::cb("🗑", format!("cm:d:{}", s.id)));
            }
            row
        })
        .collect()
}

pub fn comment_delete_confirm(id: i64) -> Inline {
    vec![vec![
        Button::cb("✅ Да, удалить", format!("cm:y:{id}")),
        Button::cb("❌ Отмена", format!("cm:n:{id}")),
    ]]
}

pub fn comment_prompt(id: i64) -> Inline {
    vec![vec![
        Button::cb("✏️ Добавить комментарий", format!("cm:a:{id}")),
        Button::cb("Позже", format!("cm:l:{id}")),
    ]]
}

pub fn clear_confirm() -> Inline {
    vec![vec![
        Button::cb("Да, удалить", "cl:y"),
        Button::cb("Отмена", "cl:n"),
    ]]
}

pub fn silence_ask() -> Inline {
    vec![
        vec![
            Button::cb("На 3 часа", "si:3h"),
            Button::cb("На 8 часов", "si:8h"),
        ],
        vec![
            Button::cb("На 24 часа", "si:24h"),
            Button::cb("До отмены", "si:forever"),
        ],
    ]
}

pub fn silence_off() -> Inline {
    vec![vec![Button::cb("🔔 Включить уведомления", "si:off")]]
}

pub fn role_select() -> Inline {
    vec![vec![
        Button::cb("👤 Я пользователь", "ro:u"),
        Button::cb("🏪 Я мерчант", "ro:m"),
    ]]
}

pub fn role_change(merchant: bool) -> Inline {
    if merchant {
        vec![vec![Button::cb("👤 Сменить на пользователя", "ro:u")]]
    } else {
        vec![vec![Button::cb("🏪 Сменить на мерчанта", "ro:m")]]
    }
}

fn yes_no(v: bool) -> &'static str {
    if v { "да" } else { "нет" }
}

pub fn settings(u: &UserRow) -> Inline {
    vec![
        vec![
            Button::cb(
                format!("Роль: {}", texts::role_label(u.is_merchant)),
                "se:ro",
            ),
            Button::cb("🔕 Тишина", "se:si"),
        ],
        vec![
            Button::cb("🌙 Тихие часы", "se:qh"),
            Button::cb("🕒 Часовой пояс", "se:tz"),
        ],
        vec![
            Button::cb(
                format!("Возврат из офлайна: {}", yes_no(u.notify_offline_return)),
                "se:or",
            ),
            Button::cb(format!("Игра: {}", yes_no(u.show_game)), "se:gm"),
        ],
        vec![
            Button::cb("📤 Экспорт", "se:ex"),
            Button::cb("🗑 Удалить мои данные", "se:fg"),
        ],
    ]
}

pub fn quiet(u: &UserRow) -> Inline {
    vec![
        vec![
            Button::cb(
                if u.quiet_enabled {
                    "Выключить"
                } else {
                    "Включить"
                },
                "qh:t",
            ),
            Button::cb("Изменить окно", "qh:e"),
        ],
        vec![Button::cb("« Настройки", "se:m")],
    ]
}

pub fn timezones() -> Inline {
    let mut rows: Inline = tz::PRESETS
        .chunks(2)
        .enumerate()
        .map(|(row, pair)| {
            pair.iter()
                .enumerate()
                .map(|(i, (city, _))| Button::cb(*city, format!("tz:{}", row * 2 + i)))
                .collect()
        })
        .collect();
    rows.push(vec![
        Button::cb("Другой", "tz:o"),
        Button::cb("« Настройки", "se:m"),
    ]);
    rows
}

pub fn back_to_settings() -> Inline {
    vec![vec![Button::cb("« Настройки", "se:m")]]
}

pub fn forget_confirm() -> Inline {
    vec![vec![
        Button::cb("Да, удалить всё", "fg:y"),
        Button::cb("Отмена", "fg:n"),
    ]]
}

pub fn broadcast_confirm() -> Inline {
    vec![vec![
        Button::cb("Отправить", "bc:y"),
        Button::cb("Отмена", "bc:n"),
    ]]
}

pub fn about(donate: bool) -> Inline {
    let mut rows = vec![vec![Button::url("⭐ GitHub", texts::REPO)]];
    let mut second = vec![Button::url("🔒 Конфиденциальность", texts::privacy_url())];
    if donate {
        second.push(Button::cb("💙 Поддержать", "dn"));
    }
    rows.push(second);
    rows
}
