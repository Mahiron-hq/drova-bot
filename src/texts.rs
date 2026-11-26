//! All user-facing texts. Messages use Telegram HTML; dynamic values go through `html()`.

use std::collections::HashMap;
use std::fmt::Write as _;

use jiff::tz::TimeZone;

use crate::db::{StationRow, Stats, SubRow, UserRow};
use crate::drova::Status;
use crate::history::{Interval, Stats as HStats};
use crate::tz;
use crate::util::{format_duration, html, station_url, users_count};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = env!("CARGO_PKG_REPOSITORY");
pub const COMMENT_MAX: usize = 500;
const MAX_HISTORY_LINES: usize = 60;

pub fn privacy_url() -> String {
    format!("{REPO}/blob/main/PRIVACY.md")
}

pub const BTN_STATUS: &str = "📊 Статус";
pub const BTN_LIST: &str = "📋 Подписки";
pub const BTN_SETTINGS: &str = "⚙️ Настройки";
pub const BTN_HELP: &str = "❓ Помощь";
pub const PLACEHOLDER: &str = "Пришлите ссылку Drova или выберите команду";

pub const NO_SUBS: &str = "Подписок пока нет. Пришлите ссылку на станцию Drova.";
pub const NOTHING_TO_DELETE: &str = "Удалять нечего: подписок пока нет.";
pub const SEND_LINK: &str = "Пришлите корректную ссылку на станцию: https://drova.io/stations/...";
pub const SUB_NOT_FOUND: &str = "Подписка не найдена, возможно, она уже удалена.";
pub const BAD_BUTTON: &str = "Кнопка устарела";
pub const ADMIN_ONLY: &str = "Команда доступна только админу.";
pub const TOO_FAST: &str = "Слишком много действий подряд, подождите немного.";
pub const API_FAILED: &str =
    "Не удалось прочитать статус станции через Drova API. Проверьте ссылку или попробуйте позже.";
pub const CANCELLED: &str = "Отменено.";
pub const INTERNAL_ERROR: &str = "Что-то пошло не так. Попробуйте ещё раз чуть позже.";

fn footer() -> String {
    "\n\nИспользуя бота, вы соглашаетесь с политикой конфиденциальности: /privacy. \
     Бот неофициальный и не связан с Drova, исходный код открыт: /about"
        .to_owned()
}

pub fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ready => "готов",
        Status::Busy => "занят",
        Status::Offline => "недоступен",
    }
}

fn history_word(s: Status) -> &'static str {
    match s {
        Status::Ready => "свободен",
        Status::Busy => "занят",
        Status::Offline => "недоступен",
    }
}

fn status_icon(s: Status, published: bool) -> &'static str {
    match s {
        Status::Ready if published => "🟢",
        Status::Ready => "🔒",
        Status::Busy => "⏳",
        Status::Offline => "🔴",
    }
}

pub fn welcome(max_user: i64) -> String {
    format!(
        "Пришлите ссылку на станцию Drova вида:\nhttps://drova.io/stations/...\n\n\
         Можно добавить до {max_user} подписок (мерчантам больше). Я слежу за станциями и пишу, \
         когда станция освобождается, а также когда она возвращается из офлайна.\n\n\
         Основные команды:\n\
         /list – мои подписки и управление ими\n\
         /status – статусы станций\n\
         /history – история и статистика\n\
         /settings – роль, тишина, тихие часы, часовой пояс\n\
         /help – подробная справка{}",
        footer()
    )
}

pub const ROLE_PROMPT: &str = "Кто вы?\n\n\
    👤 Пользователь – уведомления приходят, когда станция свободна и открыта для всех.\n\n\
    🏪 Мерчант – дополнительно уведомляет, когда станция свободна, но ещё скрыта для обычных \
    пользователей, показывает подробную статистику и может сообщать о начале сессии и уходе станции в офлайн.\n\n\
    По умолчанию установлена роль «Пользователь». Сменить роль можно в любой момент через /role.";

pub fn role_line(merchant: bool) -> String {
    format!(
        "Ваша текущая роль: {}. Сменить можно через /role.",
        role_label(merchant)
    )
}

pub fn role_label(merchant: bool) -> &'static str {
    if merchant {
        "🏪 Мерчант"
    } else {
        "👤 Пользователь"
    }
}

pub fn role_view(merchant: bool) -> String {
    if merchant {
        "Ваша текущая роль: 🏪 Мерчант\n\n\
         Вы получаете уведомления, когда станция освободилась, в том числе когда она ещё скрыта \
         для обычных пользователей.\n\nХотите сменить роль на «Пользователь»?"
            .to_owned()
    } else {
        "Ваша текущая роль: 👤 Пользователь\n\n\
         Вы получаете уведомления только когда станция свободна и открыта для всех.\n\n\
         Хотите сменить роль на «Мерчант»?"
            .to_owned()
    }
}

pub fn role_set(merchant: bool) -> String {
    if merchant {
        "✅ Роль установлена: 🏪 Мерчант\n\nВы будете получать уведомления, когда станция освободилась, \
         даже если она ещё скрыта для обычных пользователей. В карточке станции (/list) можно включить \
         уведомления о начале сессии и об уходе в офлайн."
            .to_owned()
    } else {
        "✅ Роль установлена: 👤 Пользователь\n\nВы будете получать уведомления только когда станция \
         свободна и открыта для всех."
            .to_owned()
    }
}

pub fn help(max_user: i64, max_merchant: i64) -> String {
    format!(
        "<b>Справка</b>\n\n\
         <b>Подписка.</b> Пришлите ссылку вида https://drova.io/stations/&lt;uuid&gt;. \
         Лимит: {max_user} подписок, у мерчантов {max_merchant}.\n\n\
         <b>Команды</b>\n\
         /list – подписки: карточка станции, история, комментарий, ссылка «поделиться», удаление\n\
         /status – текущие статусы станций\n\
         /history – история станции: /history 1 или /history &lt;ссылка&gt;\n\
         /settings – роль, тишина, тихие часы, часовой пояс, экспорт, удаление данных\n\
         /silence – выключить уведомления на 3 ч, 8 ч, 24 ч или до отмены\n\
         /role – сменить роль (пользователь или мерчант)\n\
         /comments – комментарии к станциям\n\
         /delete, /unsubscribe N, /remove &lt;ссылка&gt;, /clear – удаление подписок\n\
         /export и /import_subs – выгрузка и загрузка подписок с комментариями\n\
         /forget – удалить все мои данные\n\
         /about – о боте, /privacy – конфиденциальность, /donate – поддержать проект\n\n\
         <b>Когда приходит уведомление</b>\n\
         • Станция перешла из «занят» в «готов»\n\
         • Станция вернулась из офлайна (можно выключить в /settings)\n\
         • Мерчантам: станция свободна, но скрыта для обычных пользователей\n\
         • Мерчантам по желанию: станцию заняли (с игрой) и станция ушла в офлайн\n\n\
         В уведомлении видно игру последней сессии и сколько она длилась. Если станцию займут \
         в течение 5 минут, уведомление само обновится.\n\n\
         <b>Тихие часы</b> – в выбранное время уведомления приходят без звука. Время считается \
         в вашем часовом поясе (/settings).\n\n\
         <b>Статусы</b>\n🟢 готов – можно подключаться\n⏳ занят – кто-то уже в сессии\n\
         🔴 недоступен – станция не в сети\n🔒 свободна, но скрыта – видят только мерчанты\n\n\
         <b>История.</b> Всем видны переходы статуса за 24 часа с играми. Мерчантам и админу ещё \
         статистика за 7 дней: загрузка, сессии, простой, пиковые часы и топ игр.\n\n\
         Нравится бот? Поставьте ⭐ репозиторию на GitHub: {REPO}{}",
        footer()
    )
}

pub fn about(bot_username: &str) -> String {
    format!(
        "<b>Drova Bot {VERSION}</b>\n\n\
         Бот следит за станциями Drova и сообщает, когда они освобождаются.\n\n\
         Бот неофициальный и не связан с Drova. Он использует только публичные данные о станциях.\n\n\
         Исходный код открыт под лицензией GPL-3.0-or-later: {REPO}\n\
         Если бот полезен, поставьте ⭐ репозиторию, это помогает проекту.\n\n\
         Автор: Lev Burmistrov (Mahiron-hq)\nСвязь: t.me/mahiron_hq\n\
         Бот: @{}",
        html(bot_username)
    )
}

pub fn privacy() -> String {
    format!(
        "<b>Конфиденциальность</b>\n\n\
         Бот хранит только то, что нужно для работы: ваш Telegram ID, подписки на станции, \
         комментарии к ним и настройки (роль, тишина, тихие часы, часовой пояс). Имя, username, \
         телефон и содержимое переписки не сохраняются.\n\n\
         Данные хранятся на сервере в Швейцарии и никому не передаются. Неотправленные и \
         отправленные уведомления хранятся до 30 дней, история станций до 90 дней. Если вы \
         заблокируете бота, ваши данные удалятся через 90 дней.\n\n\
         Удалить все данные сразу: /forget.\n\nПолный текст политики: {}",
        privacy_url()
    )
}

pub const DONATE_MISSING: &str =
    "Способы поддержки пока не указаны. Спасибо, что пользуетесь ботом!";

// Subscriptions

fn game_line(status: Status, game: Option<&str>) -> Option<String> {
    let g = game?;
    Some(if status == Status::Busy {
        format!("🎮 Сейчас: {}", html(g))
    } else {
        format!("🎮 Последняя игра: {}", html(g))
    })
}

fn waiting_line(n: i64) -> String {
    format!("Ещё ждут освобождения: {}", users_count(n))
}

fn wait_note(s: Status) -> &'static str {
    match s {
        Status::Ready => "✅ Можно пользоваться",
        Status::Offline => "🔴 Станция недоступна, ждём возврата в онлайн",
        Status::Busy => "⏳ Ждём освобождения",
    }
}

pub struct SubView<'a> {
    pub sub: &'a SubRow,
    pub game: Option<&'a str>,
    pub waiting: i64,
}

pub fn list(items: &[SubView<'_>], limit: i64, muted: bool) -> String {
    let mut out = format!(
        "<b>Ваши подписки: {}/{limit}</b>\nРежим тишины: {}\n",
        items.len(),
        if muted {
            "включён"
        } else {
            "выключен"
        }
    );
    for (i, v) in items.iter().enumerate() {
        let st = &v.sub.station;
        let _ = write!(
            out,
            "\n{}. {} <b>{}</b> – {}\n{}",
            i + 1,
            status_icon(st.status, st.published),
            html(&st.name),
            status_word(st.status),
            wait_note(st.status)
        );
        if st.status != Status::Ready {
            let _ = write!(out, "\n{}", waiting_line(v.waiting));
        }
        if let Some(g) = game_line(st.status, v.game) {
            let _ = write!(out, "\n{g}");
        }
        if let Some(c) = &v.sub.comment {
            let _ = write!(out, "\n💬 {}", html(c));
        }
        out.push('\n');
    }
    out.push_str("\nНажмите на станцию, чтобы открыть карточку.");
    out
}

pub fn status(items: &[SubView<'_>], now: i64) -> String {
    let mut out = String::from("<b>Текущий статус станций:</b>\n");
    let mut oldest: Option<i64> = None;
    for (i, v) in items.iter().enumerate() {
        let st = &v.sub.station;
        let _ = write!(
            out,
            "\n{}. {} {} – {}",
            i + 1,
            status_icon(st.status, st.published),
            html(&st.name),
            status_word(st.status)
        );
        if st.status == Status::Ready && !st.published {
            out.push_str(" (скрыт для пользователей)");
        }
        if st.status != Status::Ready {
            let _ = write!(out, "\n{}", waiting_line(v.waiting));
        }
        if let Some(g) = game_line(st.status, v.game) {
            let _ = write!(out, "\n{g}");
        }
        out.push('\n');
        match st.checked_at {
            Some(t) => oldest = Some(oldest.map_or(t, |o: i64| o.min(t))),
            None => out.push_str("(свежих данных ещё нет)\n"),
        }
    }
    if let Some(t) = oldest {
        let _ = write!(out, "\nОбновлено {} назад.", ago(now - t));
    }
    out
}

fn ago(seconds: i64) -> String {
    if seconds < 60 {
        format!("{} с", seconds.max(0))
    } else {
        format_duration(seconds)
    }
}

pub fn card(
    sub: &SubRow,
    game: Option<&str>,
    last: Option<(Option<&str>, i64)>,
    waiting: i64,
    now: i64,
    merchant: bool,
) -> String {
    let st = &sub.station;
    let mut out = format!(
        "{} <b>{}</b>\n{}\n\nСтатус: {}",
        status_icon(st.status, st.published),
        html(&st.name),
        station_url(&st.uuid),
        status_word(st.status)
    );
    if st.status == Status::Ready && !st.published {
        out.push_str(" (скрыт для пользователей)");
    }
    let _ = write!(out, ", уже {}", format_duration(now - st.status_since));
    if st.status != Status::Ready {
        let _ = write!(out, "\n{}", waiting_line(waiting));
    }
    if let Some(g) = game_line(st.status, game) {
        let _ = write!(out, "\n{g}");
    }
    if st.status != Status::Busy
        && let Some((_, len)) = last
    {
        let _ = write!(out, "\nПоследняя сессия длилась {}", format_duration(len));
    }
    if let Some(city) = &st.city {
        let _ = write!(out, "\n📍 {}", html(city));
    }
    if let Some(n) = st.games_count {
        let _ = write!(out, "\n🕹 Игр на станции: {n}");
    }
    if let Some(c) = &sub.comment {
        let _ = write!(out, "\n💬 {}", html(c));
    }
    if merchant {
        let _ = write!(
            out,
            "\n\nУведомления мерчанта:\nНачало сессии: {}\nУход в офлайн: {}",
            on_off(sub.notify_busy),
            on_off(sub.notify_offline)
        );
    }
    out
}

pub fn on_off(v: bool) -> &'static str {
    if v { "вкл" } else { "выкл" }
}

pub fn share(name: &str, link: &str) -> String {
    format!(
        "Ссылка для подписки на станцию <b>{}</b> в один клик:\n{}\n\nЕё можно отправить друзьям или \
         добавить в описание станции на Drova.",
        html(name),
        html(link)
    )
}

pub fn delete_confirm(name: &str) -> String {
    format!("Удалить подписку на <b>{}</b>?", html(name))
}

pub fn subscribed(updated: bool, st: &StationRow, game: Option<&str>) -> String {
    let action = if updated {
        "Подписка обновлена"
    } else {
        "Подписка добавлена"
    };
    let name = html(&st.name);
    let mut out = match st.status {
        Status::Ready if st.published => format!(
            "{action}: {name}.\nТекущий статус: готов.\nЕсли станция займётся и снова освободится, я пришлю уведомление."
        ),
        Status::Ready => format!(
            "{action}: {name}.\nТекущий статус: готов (скрыт для пользователей).\n\
             Станция свободна, но сейчас скрыта для обычных пользователей. Мерчанты получат уведомление \
             при освобождении, пользователи – когда станция станет публично доступна."
        ),
        Status::Offline => format!(
            "{action}: {name}.\nТекущий статус: недоступен.\nСтанция сейчас недоступна. Уведомление придёт, когда она вернётся к работе."
        ),
        Status::Busy => format!(
            "{action}: {name}.\nТекущий статус: занят.\nУведомление придёт при освобождении. Если станция снова \
             займётся и освободится позже, уведомление придёт снова."
        ),
    };
    if let Some(g) = game_line(st.status, game) {
        let _ = write!(out, "\n{g}");
    }
    out
}

pub fn limit_reached(limit: i64, names: &[String]) -> String {
    let mut out = format!("Лимит подписок: {limit}. Ваши текущие подписки:\n");
    for (i, n) in names.iter().enumerate() {
        let _ = writeln!(out, "  {}. {}", i + 1, html(n));
    }
    out.push_str("\nУдалите одну через /list или /delete, затем добавьте новую ссылку.");
    out
}

pub const COMMENT_PROMPT_AFTER_SUB: &str =
    "Хотите добавить комментарий к этой станции?\nОн будет виден в /list и в уведомлениях.";

pub fn comment_prompt(existing: Option<&str>) -> String {
    match existing {
        Some(c) => format!(
            "Текущий комментарий: «{}»\n\nВведите новый текст (до {COMMENT_MAX} символов). Отмена: /cancel",
            html(c)
        ),
        None => {
            format!("Введите комментарий к станции (до {COMMENT_MAX} символов). Отмена: /cancel")
        }
    }
}

pub fn comment_saved(c: &str) -> String {
    format!("✅ Комментарий сохранён:\n«{}»", html(c))
}

pub fn comment_too_long(n: usize) -> String {
    format!(
        "⚠️ Слишком длинный комментарий: {n} символов.\nМаксимум – {COMMENT_MAX}. Пожалуйста, сократите и пришлите снова:"
    )
}

// History

pub fn history(
    st: &StationRow,
    day: &[Interval],
    day_start: i64,
    seven: Option<(&HStats, i64)>,
    titles: &HashMap<String, String>,
    tz: &TimeZone,
) -> String {
    let mut out = format!(
        "📜 <b>История станции: {}</b>\n{}\n\nПереходы статуса за последние 24 часа:",
        html(&st.name),
        station_url(&st.uuid)
    );
    if day.is_empty() {
        out.push_str("\nДанных по статусу за период пока нет.");
    }
    let mut lines = Vec::new();
    for (i, iv) in day.iter().enumerate() {
        let mut line = format!(
            "{} – {}",
            tz::format_time(tz, iv.start),
            history_word(iv.status)
        );
        if iv.status == Status::Busy
            && let Some(g) = iv.product_id.as_ref().and_then(|p| titles.get(p))
        {
            let _ = write!(line, " ({})", html(g));
        }
        if i == 0 && iv.start == day_start {
            line.push_str(" (состояние на начало периода)");
        }
        lines.push(line);
    }
    if lines.len() > MAX_HISTORY_LINES {
        let skipped = lines.len() - MAX_HISTORY_LINES;
        lines.drain(1..=skipped);
        if let Some(first) = lines.first_mut() {
            let _ = write!(first, "\n…пропущено переходов: {skipped}");
        }
    }
    for l in lines {
        let _ = write!(out, "\n{l}");
    }
    if let Some((s, day_idle)) = seven {
        let _ = write!(
            out,
            "\n\n<b>Статистика за последние 7 календарных дней:</b>"
        );
        if let Some(load) = s.load {
            let _ = write!(out, "\nЗагрузка (доля занятого времени в онлайне): {load}%");
        }
        let _ = write!(
            out,
            "\nСессий: {}\nСреднее время сессии: {}\nОбщее время простоя: {}\n\
             Общее время простоя за последние 24 часа: {}\nСреднее время простоя между сессиями: {}\n\
             В офлайне: {}",
            s.sessions,
            s.avg_session
                .map_or_else(|| "нет данных".to_owned(), format_duration),
            format_duration(s.ready),
            format_duration(day_idle),
            s.avg_idle_between
                .map_or_else(|| "нет данных".to_owned(), format_duration),
            format_duration(s.offline),
        );
        if !s.peak_hours.is_empty() {
            let hours: Vec<String> = s
                .peak_hours
                .iter()
                .map(|(h, p)| format!("{:02}:00-{:02}:00 ({p}%)", h, (h + 1) % 24))
                .collect();
            let _ = write!(out, "\nЧаще всего занята: {}", hours.join(", "));
        }
        if !s.top_games.is_empty() {
            out.push_str("\n\n<b>Топ игр:</b>");
            for (i, (p, n, secs)) in s.top_games.iter().enumerate() {
                let title = titles.get(p).map_or("неизвестная игра", String::as_str);
                let _ = write!(
                    out,
                    "\n{}. {} – {} {}, {}",
                    i + 1,
                    html(title),
                    n,
                    crate::util::plural(*n, "сессия", "сессии", "сессий"),
                    format_duration(*secs)
                );
            }
        }
    }
    out
}

pub const HISTORY_PICK: &str = "Выберите станцию для просмотра истории:";
pub const STATION_UNKNOWN: &str = "Такой станции нет в базе бота.";

// Notifications

pub struct FreeInfo<'a> {
    pub name: &'a str,
    pub uuid: &'a str,
    pub from_offline: bool,
    pub published: bool,
    pub game: Option<&'a str>,
    pub session: Option<i64>,
    pub comment: Option<&'a str>,
    pub others: i64,
}

pub fn free_notification(f: &FreeInfo<'_>) -> String {
    let header = match (f.published, f.from_offline) {
        (true, true) => "🟢 Станция вернулась к работе!",
        (true, false) => "🟢 Станция свободна!",
        (false, true) => "🔒 Станция вернулась к работе и доступна мерчанту!",
        (false, false) => "🔒 Станция освободилась и доступна мерчанту!",
    };
    let status = if f.published {
        "Статус: готов"
    } else {
        "Статус: свободна, но скрыта для обычных пользователей"
    };
    let mut out = format!(
        "{header}\n<b>{}</b>\n{status}\n{}",
        html(f.name),
        station_url(f.uuid)
    );
    if let Some(g) = f.game {
        let _ = write!(out, "\n🎮 Последняя игра: {}", html(g));
        if let Some(len) = f.session {
            let _ = write!(out, ", сессия длилась {}", format_duration(len));
        }
    }
    if let Some(c) = f.comment {
        let _ = write!(out, "\n💬 {}", html(c));
    }
    if f.others > 0 {
        let _ = write!(out, "\nОб этом также уведомлены {}.", users_count(f.others));
    }
    out
}

pub fn taken_suffix(status: Status, game: Option<&str>) -> String {
    match (status, game) {
        (Status::Offline, _) => "\n\n🔴 Станция ушла в офлайн.".to_owned(),
        (_, Some(g)) => format!("\n\n🔴 Уже заняли (играют в {}).", html(g)),
        _ => "\n\n🔴 Уже заняли.".to_owned(),
    }
}

/// Turns a free-station message into its "taken" variant.
pub fn mark_taken(original: &str, suffix: &str) -> String {
    let mut text = original.replacen("🟢", "🔴", 1).replacen("🔒", "🔴", 1);
    text.push_str(suffix);
    text
}

pub fn session_started(
    name: &str,
    uuid: &str,
    game: Option<&str>,
    comment: Option<&str>,
) -> String {
    let mut out = format!(
        "▶️ Станцию заняли\n<b>{}</b>\n{}",
        html(name),
        station_url(uuid)
    );
    if let Some(g) = game {
        let _ = write!(out, "\n🎮 Игра: {}", html(g));
    }
    if let Some(c) = comment {
        let _ = write!(out, "\n💬 {}", html(c));
    }
    out
}

pub fn went_offline(name: &str, uuid: &str, comment: Option<&str>) -> String {
    let mut out = format!(
        "📴 Станция ушла в офлайн\n<b>{}</b>\n{}",
        html(name),
        station_url(uuid)
    );
    if let Some(c) = comment {
        let _ = write!(out, "\n💬 {}", html(c));
    }
    out
}

pub fn unmuted_report(status_text: &str) -> String {
    format!(
        "Режим тишины отключен. Ниже текущие состояния станций, чтобы вы не пропустили изменения за время паузы.\n\n{status_text}"
    )
}

pub const STILL_FREE: &str = "Станция всё ещё свободна ✅";
pub const NO_DATA_YET: &str = "Данные ещё не загружены, попробуйте позже";

// Settings

pub fn settings(u: &UserRow, now: i64) -> String {
    let mute = if u.mute_active(now) {
        match u.mute_until {
            Some(t) => format!("до {}", tz::format_date_time(&tz::resolve(&u.tz), t)),
            None => "бессрочно".to_owned(),
        }
    } else {
        "выключена".to_owned()
    };
    let quiet = if u.quiet_enabled {
        format!(
            "{}-{} (без звука)",
            tz::hhmm(u.quiet_start),
            tz::hhmm(u.quiet_end)
        )
    } else {
        "выключены".to_owned()
    };
    format!(
        "⚙️ <b>Настройки</b>\n\n\
         Роль: {}\nТишина: {mute}\nТихие часы: {quiet}\nЧасовой пояс: {}\n\
         Уведомлять о возврате из офлайна: {}\nПоказывать игру в уведомлениях: {}",
        role_label(u.is_merchant),
        html(&tz::label(&u.tz)),
        if u.notify_offline_return {
            "да"
        } else {
            "нет"
        },
        if u.show_game { "да" } else { "нет" },
    )
}

pub fn silence_on(remaining: Option<i64>) -> String {
    match remaining {
        Some(s) => format!(
            "🔕 Тишина включена. Осталось: {}. Хотите включить уведомления?",
            format_duration(s)
        ),
        None => "🔕 Тишина включена бессрочно. Хотите включить уведомления?".to_owned(),
    }
}

pub const SILENCE_ASK: &str = "🔕 На какое время выключить уведомления?";

pub fn silence_set(hours: Option<i64>) -> String {
    match hours {
        Some(h) => format!("🔕 Тишина включена на {h} ч. Подписки сохранены."),
        None => "🔕 Тишина включена до вашей отмены. Подписки сохранены.".to_owned(),
    }
}

pub fn quiet_view(u: &UserRow) -> String {
    format!(
        "🌙 <b>Тихие часы</b>\n\nВ это время уведомления приходят без звука.\n\
         Окно: {}-{}\nСостояние: {}\nЧасовой пояс: {}",
        tz::hhmm(u.quiet_start),
        tz::hhmm(u.quiet_end),
        if u.quiet_enabled {
            "включены"
        } else {
            "выключены"
        },
        html(&tz::label(&u.tz))
    )
}

pub const QUIET_ASK: &str =
    "Пришлите окно тихих часов, например <code>23:00-08:00</code>. Отмена: /cancel";
pub const QUIET_BAD: &str = "Не понял время. Пример: <code>23:00-08:00</code>. Отмена: /cancel";

pub fn tz_view(current: &str) -> String {
    format!(
        "🕒 <b>Часовой пояс</b>\n\nСейчас: {}\nВыберите город или нажмите «Другой».",
        html(&tz::label(current))
    )
}

pub const TZ_ASK: &str = "Пришлите часовой пояс: название вроде <code>Europe/Berlin</code> \
    или смещение вроде <code>UTC+5</code>. Отмена: /cancel";
pub const TZ_BAD: &str = "Не нашёл такой часовой пояс. Примеры: <code>Asia/Almaty</code>, <code>UTC+3</code>. Отмена: /cancel";

pub const FORGET_ASK: &str = "⚠️ Удалить все ваши данные: подписки, комментарии и настройки? \
    Это действие нельзя отменить.";
pub const FORGET_DONE: &str =
    "✅ Все ваши данные удалены. Если захотите вернуться, просто пришлите ссылку на станцию.";

// Import and export

pub fn export_caption(n: usize) -> String {
    format!(
        "Ваши подписки ({n} шт.) с комментариями. Чтобы перенести их на другой аккаунт, используйте /import_subs."
    )
}

pub fn import_ask(count: i64, limit: i64) -> String {
    format!(
        "Пришлите список ссылок Drova для импорта, по одной на строке. Комментарий можно указать после \
         знака #, как в файле из /export.\n\nТакже можно прикрепить .txt-файл, выгруженный через /export.\n\n\
         Сейчас у вас {count}/{limit} подписок. Отправьте /cancel, чтобы отменить."
    )
}

pub const IMPORT_NONE: &str = "Не найдено ни одной корректной ссылки Drova. Ссылки должны иметь вид \
    https://drova.io/stations/&lt;uuid&gt;.";
pub const IMPORT_BAD_FILE: &str = "⚠️ Ожидается текстовый файл (.txt). Прикрепите файл, выгруженный через \
    /export, или отправьте ссылки текстом. Для отмены – /cancel.";
pub const IMPORT_DOWNLOAD_FAILED: &str =
    "❌ Не удалось загрузить файл. Попробуйте ещё раз или отправьте ссылки текстом.";

pub fn import_too_many(new: usize, free: i64, count: i64, limit: i64) -> String {
    format!(
        "В списке {new} новых ссылок, а свободных мест – {free} (у вас {count}/{limit} подписок).\n\n\
         Сократите список или освободите место через /list и повторите импорт."
    )
}

pub fn import_report(
    added: &[String],
    updated: &[String],
    failed: usize,
    rate_limited: usize,
) -> String {
    let mut lines = Vec::new();
    if !added.is_empty() {
        lines.push(format!(
            "✅ Добавлено ({}): {}",
            added.len(),
            html(&added.join(", "))
        ));
    }
    if !updated.is_empty() {
        lines.push(format!(
            "🔄 Обновлено ({}): {}",
            updated.len(),
            html(&updated.join(", "))
        ));
    }
    if rate_limited > 0 {
        lines.push(format!(
            "⏱ Слишком частый запрос, пропущено: {rate_limited}. Повторите через 30 с."
        ));
    }
    if failed > 0 {
        lines.push(format!(
            "❌ Не удалось получить статус ({failed} ссылок), пропущены."
        ));
    }
    if lines.is_empty() {
        lines.push("Ничего не изменилось.".to_owned());
    }
    lines.join("\n")
}

// Admin

pub fn admin(
    s: &Stats,
    uptime: i64,
    last_cycle: Option<(i64, i64, usize, usize)>,
    api_down: bool,
    errors: &[(i64, String, String)],
    tz: &TimeZone,
    test_mode: bool,
) -> String {
    let mut out = format!(
        "<b>Админ-статистика</b> (версия {VERSION}{})\n\n\
         Пользователей: {} (с подписками {}, заблокировали бота {})\n\
         Мерчантов: {}\nВ тишине: {}\nПодписок: {}\nСтанций: {}\n\
         Очередь сообщений: {}\nОшибок опроса за 24 ч: {}\nАптайм: {}",
        if test_mode {
            ", тестовый режим"
        } else {
            ""
        },
        s.users,
        s.active_users,
        s.blocked_users,
        s.merchants,
        s.muted,
        s.subscriptions,
        s.stations,
        s.queue_pending,
        s.errors_24h,
        format_duration(uptime),
    );
    if let Some((at, ms, ok, total)) = last_cycle {
        let _ = write!(
            out,
            "\nПоследний опрос: {}, {ok}/{total} станций, {} мс",
            tz::format_date_time(tz, at),
            ms
        );
    }
    if api_down {
        out.push_str("\n⚠️ API Drova сейчас не отвечает");
    }
    if errors.is_empty() {
        out.push_str("\n\nОшибок нет.");
    } else {
        out.push_str("\n\n<b>Последние ошибки:</b>");
        for (at, name, err) in errors {
            let _ = write!(
                out,
                "\n• {} | {} | {}",
                tz::format_date_time(tz, *at),
                html(&crate::util::clip(name, 40)),
                html(&crate::util::clip(err, 200))
            );
        }
    }
    out
}

pub const BROADCAST_ASK: &str =
    "Пришлите текст рассылки (обычный текст, до 3500 символов). Отмена: /cancel";

pub fn broadcast_preview(text: &str, n: usize) -> String {
    format!(
        "Предпросмотр рассылки для {}:\n\n{}",
        users_count(n as i64),
        html(text)
    )
}

pub fn broadcast_queued(n: usize) -> String {
    format!(
        "✅ Рассылка поставлена в очередь: {}.",
        users_count(n as i64)
    )
}

pub fn admin_digest(
    down: &[(String, String)],
    up: &[String],
    now_utc: &str,
    fail_streak: i64,
) -> String {
    let mut out = String::from("Drova: опрос станций\n");
    if !down.is_empty() {
        let _ = write!(out, "\nНе отвечают {fail_streak}+ опроса подряд:");
        for (name, err) in down {
            let _ = write!(
                out,
                "\n• {}\n  {}",
                html(name),
                html(&crate::util::clip(err, 300))
            );
        }
        out.push('\n');
    }
    if !up.is_empty() {
        out.push_str("\nСнова отвечают:");
        for name in up {
            let _ = write!(out, "\n• {}", html(name));
        }
        out.push('\n');
    }
    let _ = write!(out, "\nВремя UTC: {now_utc}");
    out
}

pub fn api_down(err: &str) -> String {
    format!(
        "⚠️ API Drova не отвечает: все станции подряд выдают ошибку.\n{}\nОпрос продолжается с паузами.",
        html(&crate::util::clip(err, 300))
    )
}

pub const API_UP: &str = "✅ API Drova снова отвечает, опрос идёт в обычном режиме.";

pub fn unknown_state(raw: &str, name: &str) -> String {
    format!(
        "ℹ️ Drova вернула неизвестный статус станции: <code>{}</code> ({}). Он считается «недоступен».",
        html(raw),
        html(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taken_edit_replaces_icon_once() {
        let t = mark_taken(
            "🟢 Станция свободна!\n<b>A</b>",
            &taken_suffix(Status::Busy, Some("Dota 2")),
        );
        assert!(t.starts_with("🔴 Станция свободна!"));
        assert!(t.ends_with("(играют в Dota 2)."));
    }

    #[test]
    fn free_notification_escapes() {
        let text = free_notification(&FreeInfo {
            name: "<PC>",
            uuid: "u",
            from_offline: false,
            published: true,
            game: Some("A&B"),
            session: Some(3600),
            comment: None,
            others: 2,
        });
        assert!(text.contains("&lt;PC&gt;"));
        assert!(text.contains("A&amp;B, сессия длилась 1 ч"));
        assert!(text.contains("2 пользователя"));
    }

    #[test]
    fn no_em_dash_in_texts() {
        let all = [
            welcome(5),
            help(5, 20),
            about("bot"),
            privacy(),
            ROLE_PROMPT.to_owned(),
        ];
        for t in all {
            assert!(!t.contains('\u{2014}'), "{t}");
        }
    }
}
