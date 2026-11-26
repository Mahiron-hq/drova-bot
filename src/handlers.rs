use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::app::{App, CommentOrigin, Dialog};
use crate::db::{AddResult, Outgoing, StationRow, SubFlag, UserFlag, UserRow};
use crate::drova::Status;
use crate::history;
use crate::keyboards as kb;
use crate::telegram::{CallbackQuery, Inline, Markup, Message, TgError, Update};
use crate::texts::{self, SubView};
use crate::tz;
use crate::util::{clip, extract_station_ids, first_station_id, html, is_uuid, now, station_url};

const REFRESH_COOLDOWN: i64 = 5;
const SUBSCRIBE_COOLDOWN: i64 = 30;
const FRESH_SNAPSHOT: i64 = 60;
/// Destructive confirmation buttons stop working after this many seconds.
const CONFIRM_TTL: i64 = 600;

pub async fn handle(app: Arc<App>, upd: Update) {
    let user_id = upd
        .message
        .as_ref()
        .and_then(|m| m.from.as_ref().map(|f| f.id))
        .or_else(|| upd.callback_query.as_ref().map(|c| c.from.id))
        .or_else(|| upd.my_chat_member.as_ref().map(|m| m.from.id));
    let Some(user_id) = user_id else { return };
    let lock = app.user_lock(user_id).await;
    let _guard = lock.lock().await;
    let result = if let Some(m) = upd.my_chat_member {
        chat_member(&app, &m).await
    } else if let Some(m) = upd.message {
        message(&app, m).await
    } else if let Some(q) = upd.callback_query {
        callback(&app, q).await
    } else {
        Ok(())
    };
    if let Err(e) = result {
        tracing::error!(user_id, error = format!("{e:#}"), "update handling failed");
    }
}

async fn chat_member(app: &App, m: &crate::telegram::ChatMemberUpdated) -> Result<()> {
    if m.chat.kind != "private" {
        return Ok(());
    }
    match m.new_chat_member.status.as_str() {
        "kicked" => {
            tracing::info!(user_id = m.from.id, "user blocked the bot");
            app.db.mark_blocked(m.from.id, now()).await?;
            app.db.drop_pending_for_chat(m.from.id, now()).await?;
        }
        "member" => app.db.touch_user(m.from.id, now()).await?,
        _ => {}
    }
    Ok(())
}

async fn send(app: &App, chat: i64, text: &str, markup: Option<&Markup>) {
    if let Err(e) = app.tg.send(chat, text, markup, false).await
        && !matches!(e, TgError::Suppressed)
    {
        tracing::warn!(chat, error = %e, "cannot send reply");
    }
}

async fn send_inline(app: &App, chat: i64, text: &str, inline: Inline) {
    send(app, chat, text, Some(&Markup::Inline(inline))).await;
}

async fn edit(app: &App, chat: i64, msg: i64, text: &str, inline: Option<Inline>) {
    if let Err(e) = app.tg.edit(chat, msg, text, inline.as_ref()).await
        && !matches!(e, TgError::Suppressed)
    {
        tracing::debug!(chat, error = %e, "cannot edit message");
        send(app, chat, text, inline.map(Markup::Inline).as_ref()).await;
    }
}

async fn message(app: &App, m: Message) -> Result<()> {
    if m.chat.kind != "private" {
        return Ok(());
    }
    let Some(from) = m.from.as_ref().filter(|f| !f.is_bot) else {
        return Ok(());
    };
    let chat = m.chat.id;
    app.db.touch_user(from.id, now()).await?;
    match app.flood_check(from.id).await {
        None => {}
        Some(true) => {
            send(app, chat, texts::TOO_FAST, None).await;
            return Ok(());
        }
        Some(false) => return Ok(()),
    }
    let u = app.user_or_default(from.id).await?;
    let result = message_inner(app, &u, chat, &m).await;
    if let Err(e) = &result {
        tracing::error!(
            user_id = u.id,
            error = format!("{e:#}"),
            "message handler failed"
        );
        send(app, chat, texts::INTERNAL_ERROR, None).await;
    }
    Ok(())
}

async fn message_inner(app: &App, u: &UserRow, chat: i64, m: &Message) -> Result<()> {
    if let Some(doc) = &m.document {
        if matches!(app.dialog(u.id).await, Some(Dialog::Import)) {
            return import_file(app, u, chat, doc).await;
        }
        send(
            app,
            chat,
            "Чтобы загрузить подписки из файла, используйте /import_subs.",
            None,
        )
        .await;
        return Ok(());
    }
    let Some(text) = m.text.as_deref().map(str::trim) else {
        return Ok(());
    };
    if let Some(rest) = text.strip_prefix('/') {
        let (head, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let cmd = head
            .split('@')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if cmd != "cancel" {
            app.clear_dialog(u.id).await;
        }
        return command(app, u, chat, &cmd, arg.trim()).await;
    }
    let by_button = match text {
        texts::BTN_STATUS => Some("status"),
        texts::BTN_LIST => Some("list"),
        texts::BTN_SETTINGS => Some("settings"),
        texts::BTN_HELP => Some("help"),
        _ => None,
    };
    if let Some(cmd) = by_button {
        app.clear_dialog(u.id).await;
        return command(app, u, chat, cmd, "").await;
    }
    if let Some(d) = app.dialog(u.id).await {
        return dialog_input(app, u, chat, d, text).await;
    }
    let mut ids = extract_station_ids(text);
    match ids.len() {
        0 => {
            send(app, chat, texts::SEND_LINK, Some(&kb::main())).await;
            Ok(())
        }
        1 => subscribe(app, u, chat, &ids.remove(0)).await,
        _ => import_items(app, u, chat, ids.into_iter().map(|id| (id, None)).collect()).await,
    }
}

async fn command(app: &App, u: &UserRow, chat: i64, cmd: &str, arg: &str) -> Result<()> {
    tracing::info!(user_id = u.id, cmd, "command");
    match cmd {
        "start" => start(app, u, chat, arg).await,
        "help" => {
            let text = texts::help(app.cfg.max_subs_user, app.cfg.max_subs_merchant);
            send(app, chat, &text, Some(&kb::main())).await;
            Ok(())
        }
        "about" => {
            send_inline(
                app,
                chat,
                &texts::about(&app.bot_username),
                kb::about(app.cfg.donate_text.is_some()),
            )
            .await;
            Ok(())
        }
        "privacy" => {
            send(app, chat, &texts::privacy(), None).await;
            Ok(())
        }
        "donate" => {
            send(
                app,
                chat,
                app.cfg
                    .donate_text
                    .as_deref()
                    .unwrap_or(texts::DONATE_MISSING),
                None,
            )
            .await;
            Ok(())
        }
        "list" => list(app, u, chat).await,
        "status" => status(app, u, chat).await,
        "history" => history_cmd(app, u, chat, arg).await,
        "settings" => {
            send_inline(app, chat, &texts::settings(u, now()), kb::settings(u)).await;
            Ok(())
        }
        "silence" => {
            silence_view(app, u, chat, None).await;
            Ok(())
        }
        "role" => {
            send_inline(
                app,
                chat,
                &texts::role_view(u.is_merchant),
                kb::role_change(u.is_merchant),
            )
            .await;
            Ok(())
        }
        "comments" => {
            let subs = app.db.subs_for_user(u.id).await?;
            if subs.is_empty() {
                send(app, chat, texts::NO_SUBS, Some(&kb::main())).await;
            } else {
                send_inline(app, chat, "Комментарии к станциям:", kb::comments(&subs)).await;
            }
            Ok(())
        }
        "delete" => {
            let subs = app.db.subs_for_user(u.id).await?;
            if subs.is_empty() {
                send(app, chat, texts::NOTHING_TO_DELETE, Some(&kb::main())).await;
            } else {
                send_inline(
                    app,
                    chat,
                    "Выберите подписку для удаления:",
                    kb::pick(&subs, "d"),
                )
                .await;
            }
            Ok(())
        }
        "unsubscribe" => unsubscribe(app, u, chat, arg).await,
        "remove" => remove(app, u, chat, arg).await,
        "clear" => {
            let n = app.db.sub_count(u.id).await?;
            if n == 0 {
                send(app, chat, texts::NOTHING_TO_DELETE, Some(&kb::main())).await;
            } else {
                send_inline(
                    app,
                    chat,
                    &format!("Удалить все подписки? Сейчас их {n}."),
                    kb::clear_confirm(),
                )
                .await;
            }
            Ok(())
        }
        "export" => export(app, u, chat).await,
        "import_subs" | "import" => {
            let count = app.db.sub_count(u.id).await?;
            let limit = app.limit_for(u);
            app.set_dialog(u.id, Dialog::Import).await;
            send(app, chat, &texts::import_ask(count, limit), None).await;
            Ok(())
        }
        "cancel" => {
            app.clear_dialog(u.id).await;
            send(app, chat, texts::CANCELLED, Some(&kb::main())).await;
            Ok(())
        }
        "forget" => {
            send_inline(app, chat, texts::FORGET_ASK, kb::forget_confirm()).await;
            Ok(())
        }
        "admin" => admin(app, u, chat).await,
        "broadcast" => {
            if app.cfg.is_admin(u.id) {
                app.set_dialog(u.id, Dialog::Broadcast).await;
                send(app, chat, texts::BROADCAST_ASK, None).await;
            } else {
                send(app, chat, texts::ADMIN_ONLY, None).await;
            }
            Ok(())
        }
        _ => {
            send(
                app,
                chat,
                "Неизвестная команда. Справка: /help",
                Some(&kb::main()),
            )
            .await;
            Ok(())
        }
    }
}

async fn start(app: &App, u: &UserRow, chat: i64, arg: &str) -> Result<()> {
    let param = arg.trim().to_ascii_lowercase();
    if is_uuid(&param) {
        send(
            app,
            chat,
            &texts::welcome(app.cfg.max_subs_user),
            Some(&kb::main()),
        )
        .await;
        return subscribe(app, u, chat, &param).await;
    }
    send(
        app,
        chat,
        &texts::welcome(app.cfg.max_subs_user),
        Some(&kb::main()),
    )
    .await;
    if u.role_selected {
        send(app, chat, &texts::role_line(u.is_merchant), None).await;
    } else {
        send_inline(app, chat, texts::ROLE_PROMPT, kb::role_select()).await;
    }
    Ok(())
}

async fn list_view(app: &App, u: &UserRow) -> Result<Option<(String, Inline)>> {
    let (subs, titles, waiting) = app.sub_views(u).await?;
    if subs.is_empty() {
        return Ok(None);
    }
    let views: Vec<SubView<'_>> = subs
        .iter()
        .map(|s| SubView {
            sub: s,
            game: s
                .station
                .product_id
                .as_ref()
                .and_then(|p| titles.get(p))
                .map(String::as_str),
            waiting: waiting.get(&s.station.uuid).copied().unwrap_or(0),
        })
        .collect();
    let text = texts::list(
        &views,
        app.limit_for(u),
        u.mute_active(now()),
        u.is_merchant,
        now(),
    );
    Ok(Some((text, kb::list(&subs))))
}

async fn list(app: &App, u: &UserRow, chat: i64) -> Result<()> {
    app.tg.typing(chat).await;
    match list_view(app, u).await? {
        Some((text, inline)) => send_inline(app, chat, &text, inline).await,
        None => send(app, chat, texts::NO_SUBS, Some(&kb::main())).await,
    }
    Ok(())
}

async fn status(app: &App, u: &UserRow, chat: i64) -> Result<()> {
    match app.status_text(u).await? {
        Some(text) => send_inline(app, chat, &text, kb::status()).await,
        None => send(app, chat, texts::NO_SUBS, Some(&kb::main())).await,
    }
    Ok(())
}

async fn card_view(app: &App, u: &UserRow, id: i64) -> Result<Option<(String, Inline)>> {
    let Some(sub) = app.db.sub(u.id, id).await? else {
        return Ok(None);
    };
    let game = match (&sub.station.product_id, u.show_game) {
        (Some(p), true) => app.titles(std::slice::from_ref(p)).await.remove(p),
        _ => None,
    };
    let last = app.db.last_session(&sub.station.uuid).await?;
    let waiting = app
        .db
        .waiting_counts(u.id, now())
        .await?
        .get(&sub.station.uuid)
        .copied()
        .unwrap_or(0);
    let hardware = if sub.station.gone_at.is_none() {
        app.hardware(&sub.station.uuid).await
    } else {
        None
    };
    let text = texts::card(&texts::CardInfo {
        sub: &sub,
        game: game.as_deref(),
        last: last.as_ref().map(|(g, l)| (g.as_deref(), *l)),
        waiting,
        hardware: hardware.as_ref(),
        now: now(),
        merchant: u.is_merchant,
    });
    Ok(Some((text, kb::card(&sub, u.is_merchant))))
}

async fn history_text(app: &App, u: &UserRow, st: &StationRow) -> Result<String> {
    let t = now();
    let zone = tz::resolve(&u.tz);
    let day_start = t - 86_400;
    let day_rows = app.db.transitions(&st.uuid, day_start, t).await?;
    let day = history::intervals(&day_rows, day_start, t);
    let mut ids: HashSet<String> = day.iter().filter_map(|i| i.product_id.clone()).collect();
    let seven = if app.can_see_stats(u) {
        let start = history::calendar_start(t, 7, &zone);
        let rows = app.db.transitions(&st.uuid, start, t).await?;
        let s = history::stats(&history::intervals(&rows, start, t), &zone);
        ids.extend(s.top_games.iter().map(|g| g.0.clone()));
        let day_idle: i64 = day
            .iter()
            .filter(|i| i.status == Status::Ready)
            .map(|i| i.end - i.start)
            .sum();
        Some((s, day_idle))
    } else {
        None
    };
    let ids: Vec<String> = ids.into_iter().collect();
    let titles = app.titles(&ids).await;
    Ok(texts::history(
        st,
        &day,
        day_start,
        seven.as_ref().map(|(s, d)| (s, *d)),
        &titles,
        &zone,
    ))
}

async fn history_cmd(app: &App, u: &UserRow, chat: i64, arg: &str) -> Result<()> {
    let subs = app.db.subs_for_user(u.id).await?;
    let uuid = if arg.is_empty() {
        if subs.is_empty() {
            send(app, chat, texts::NO_SUBS, Some(&kb::main())).await;
        } else {
            send_inline(app, chat, texts::HISTORY_PICK, kb::pick(&subs, "h")).await;
        }
        return Ok(());
    } else if let Ok(n) = arg.parse::<usize>() {
        match n.checked_sub(1).and_then(|i| subs.get(i)) {
            Some(s) => s.station.uuid.clone(),
            None => {
                send(
                    app,
                    chat,
                    &format!("Нет подписки с номером {n}. Проверьте список через /list."),
                    None,
                )
                .await;
                return Ok(());
            }
        }
    } else if let Some(id) = first_station_id(arg) {
        id
    } else {
        send(
            app,
            chat,
            "Не удалось распознать станцию. Используйте /history N или /history &lt;ссылка&gt;.",
            None,
        )
        .await;
        return Ok(());
    };
    app.tg.typing(chat).await;
    match app.db.station(&uuid).await? {
        Some(st) => {
            let text = history_text(app, u, &st).await?;
            send(app, chat, &text, None).await;
        }
        None => send(app, chat, texts::STATION_UNKNOWN, None).await,
    }
    Ok(())
}

async fn silence_view(app: &App, u: &UserRow, chat: i64, edit_msg: Option<i64>) {
    let t = now();
    let (text, inline) = if u.mute_active(t) {
        (
            texts::silence_on(u.mute_until.map(|x| x - t)),
            kb::silence_off(),
        )
    } else {
        (texts::SILENCE_ASK.to_owned(), kb::silence_ask())
    };
    match edit_msg {
        Some(m) => edit(app, chat, m, &text, Some(inline)).await,
        None => send_inline(app, chat, &text, inline).await,
    }
}

async fn unsubscribe(app: &App, u: &UserRow, chat: i64, arg: &str) -> Result<()> {
    let Some(n) = arg.parse::<usize>().ok().filter(|n| *n >= 1) else {
        send(
            app,
            chat,
            "Укажите номер подписки из /list, например: /unsubscribe 2",
            None,
        )
        .await;
        return Ok(());
    };
    let subs = app.db.subs_for_user(u.id).await?;
    if subs.is_empty() {
        send(app, chat, texts::NOTHING_TO_DELETE, None).await;
        return Ok(());
    }
    let Some(s) = subs.get(n - 1) else {
        send(
            app,
            chat,
            &format!(
                "Нет подписки с номером {n}. У вас подписок: {}. Список: /list",
                subs.len()
            ),
            None,
        )
        .await;
        return Ok(());
    };
    let text = if app.db.remove_sub(u.id, s.id).await? {
        format!("Подписка #{n} удалена: {}", html(&s.station.name))
    } else {
        "Подписка уже была удалена.".to_owned()
    };
    send(app, chat, &text, None).await;
    Ok(())
}

async fn remove(app: &App, u: &UserRow, chat: i64, arg: &str) -> Result<()> {
    if let Some(uuid) = first_station_id(arg) {
        let text = if app.db.remove_sub_by_station(u.id, &uuid).await? {
            "Подписка удалена."
        } else {
            "Такой подписки не было."
        };
        send(app, chat, text, None).await;
        return Ok(());
    }
    if app.db.sub_count(u.id).await? == 0 {
        send(app, chat, texts::NOTHING_TO_DELETE, None).await;
        return Ok(());
    }
    app.set_dialog(u.id, Dialog::RemoveUrl).await;
    send(
        app,
        chat,
        "Отправьте ссылку на станцию, которую хотите удалить (или /list, чтобы удалить кнопкой). Отмена: /cancel",
        None,
    )
    .await;
    Ok(())
}

async fn export(app: &App, u: &UserRow, chat: i64) -> Result<()> {
    let subs = app.db.subs_for_user(u.id).await?;
    if subs.is_empty() {
        send(app, chat, "Подписок нет, выгружать нечего.", None).await;
        return Ok(());
    }
    let mut body = String::new();
    for s in &subs {
        body.push_str(&station_url(&s.station.uuid));
        if let Some(c) = &s.comment {
            body.push_str(" # ");
            body.push_str(&c.replace(['\n', '\r'], " "));
        }
        body.push('\n');
    }
    let name = format!("drova_subscriptions_{}.txt", u.id);
    if let Err(e) = app
        .tg
        .send_document(
            chat,
            &name,
            body.into_bytes(),
            &texts::export_caption(subs.len()),
        )
        .await
        && !matches!(e, TgError::Suppressed)
    {
        tracing::warn!(error = %e, "cannot send export");
    }
    Ok(())
}

async fn admin(app: &App, u: &UserRow, chat: i64) -> Result<()> {
    if !app.cfg.is_admin(u.id) {
        send(app, chat, texts::ADMIN_ONLY, None).await;
        return Ok(());
    }
    let t = now();
    let stats = app.db.stats(t).await?;
    let errors = app.db.recent_errors(10).await?;
    let cycle = app.health.cycle.lock().ok().and_then(|c| *c);
    let text = texts::admin(
        &stats,
        t - app
            .health
            .started
            .load(std::sync::atomic::Ordering::Relaxed),
        cycle,
        app.health
            .api_down
            .load(std::sync::atomic::Ordering::Relaxed),
        &errors,
        &tz::resolve(&u.tz),
        app.tg.test_mode(),
    );
    send(app, chat, &text, None).await;
    Ok(())
}

/// Snapshot from the monitor cache when fresh, otherwise a live request.
async fn fresh_station(app: &App, uuid: &str) -> Option<StationRow> {
    let cached = app.db.station(uuid).await.ok().flatten();
    if let Some(st) = &cached
        && st.checked_at.is_some_and(|c| now() - c < FRESH_SNAPSHOT)
    {
        return cached;
    }
    match app.drova.station(uuid).await {
        Ok(snap) => {
            if let Some(p) = &snap.product_id {
                app.title(p).await;
            }
            let on_return = if cached.as_ref().is_some_and(|s| s.gone_at.is_some()) {
                crate::monitor::returned_notices(app, uuid, &snap.name)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let a = match app.db.apply_snapshot(&snap, now(), &on_return).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(uuid, error = format!("{e:#}"), "cannot store station");
                    return None;
                }
            };
            if a.returned {
                app.wake_queue();
            }
            Some(a.new)
        }
        Err(e) => {
            tracing::info!(uuid, error = %e, "station lookup failed");
            None
        }
    }
}

async fn subscribe(app: &App, u: &UserRow, chat: i64, uuid: &str) -> Result<()> {
    let key = format!("sub:{uuid}");
    if let Err(left) = app.cooldown(u.id, &key, SUBSCRIBE_COOLDOWN).await {
        send(
            app,
            chat,
            &format!("Эта ссылка уже обрабатывается. Повторите через {left} с."),
            None,
        )
        .await;
        return Ok(());
    }
    app.tg.typing(chat).await;
    let Some(st) = fresh_station(app, uuid).await else {
        app.reset_cooldown(u.id, &key).await;
        send(app, chat, texts::API_FAILED, Some(&kb::main())).await;
        return Ok(());
    };
    let limit = app.limit_for(u);
    let result = app
        .db
        .add_subscription(u.id, uuid, u.is_merchant, limit, now())
        .await?;
    let (id, updated) = match result {
        AddResult::Limit => {
            let names: Vec<String> = app
                .db
                .subs_for_user(u.id)
                .await?
                .into_iter()
                .map(|s| s.station.name)
                .collect();
            send(
                app,
                chat,
                &texts::limit_reached(limit, &names),
                Some(&kb::main()),
            )
            .await;
            return Ok(());
        }
        AddResult::Added(id) => (id, false),
        AddResult::Updated(id) => (id, true),
    };
    let game = match (&st.product_id, u.show_game) {
        (Some(p), true) => app.title(p).await,
        _ => None,
    };
    send(
        app,
        chat,
        &texts::subscribed(updated, &st, game.as_deref()),
        Some(&kb::main()),
    )
    .await;
    if app
        .db
        .sub(u.id, id)
        .await?
        .is_some_and(|s| s.comment.is_none())
    {
        send_inline(
            app,
            chat,
            texts::COMMENT_PROMPT_AFTER_SUB,
            kb::comment_prompt(id),
        )
        .await;
    }
    Ok(())
}

/// Lines of an import: links with an optional `# comment` after a single link.
pub fn parse_import(text: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for line in text.lines() {
        let (links, comment) = match line.split_once('#') {
            Some((l, c)) => (l, Some(c.trim())),
            None => (line, None),
        };
        let ids = extract_station_ids(links);
        let single = ids.len() == 1;
        for id in ids {
            if out.iter().any(|(x, _)| *x == id) {
                continue;
            }
            let c = comment
                .filter(|c| single && !c.is_empty())
                .map(|c| c.chars().take(texts::COMMENT_MAX).collect());
            out.push((id, c));
        }
    }
    out
}

async fn import_items(
    app: &App,
    u: &UserRow,
    chat: i64,
    items: Vec<(String, Option<String>)>,
) -> Result<()> {
    if items.is_empty() {
        send(app, chat, texts::IMPORT_NONE, None).await;
        return Ok(());
    }
    let subs = app.db.subs_for_user(u.id).await?;
    let existing: HashSet<&str> = subs.iter().map(|s| s.station.uuid.as_str()).collect();
    let limit = app.limit_for(u);
    let count = subs.len() as i64;
    let new = items
        .iter()
        .filter(|(id, _)| !existing.contains(id.as_str()))
        .count();
    if new as i64 > limit - count {
        send(
            app,
            chat,
            &texts::import_too_many(new, (limit - count).max(0), count, limit),
            None,
        )
        .await;
        return Ok(());
    }
    app.tg.typing(chat).await;
    let (mut added, mut updated) = (Vec::new(), Vec::new());
    let (mut failed, mut rate_limited) = (0, 0);
    for (uuid, comment) in items {
        let key = format!("sub:{uuid}");
        if app.cooldown(u.id, &key, SUBSCRIBE_COOLDOWN).await.is_err() {
            rate_limited += 1;
            continue;
        }
        let Some(st) = fresh_station(app, &uuid).await else {
            app.reset_cooldown(u.id, &key).await;
            failed += 1;
            continue;
        };
        match app
            .db
            .add_subscription(u.id, &uuid, u.is_merchant, limit, now())
            .await?
        {
            AddResult::Added(_) => added.push(st.name.clone()),
            AddResult::Updated(_) => updated.push(st.name.clone()),
            AddResult::Limit => {
                failed += 1;
                continue;
            }
        }
        if let Some(c) = comment {
            app.db
                .set_comment_by_station(u.id, &uuid, &c, now())
                .await?;
        }
    }
    send(
        app,
        chat,
        &texts::import_report(&added, &updated, failed, rate_limited),
        Some(&kb::main()),
    )
    .await;
    Ok(())
}

async fn import_file(
    app: &App,
    u: &UserRow,
    chat: i64,
    doc: &crate::telegram::Document,
) -> Result<()> {
    let mime_ok = doc
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("text/plain"));
    let name_ok = doc
        .file_name
        .as_deref()
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".txt"));
    if !mime_ok && !name_ok || doc.file_size.unwrap_or(0) > 512 * 1024 {
        send(app, chat, texts::IMPORT_BAD_FILE, None).await;
        return Ok(());
    }
    let bytes = match app.tg.download(&doc.file_id).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "import download failed");
            app.clear_dialog(u.id).await;
            send(app, chat, texts::IMPORT_DOWNLOAD_FAILED, None).await;
            return Ok(());
        }
    };
    app.clear_dialog(u.id).await;
    let text = String::from_utf8_lossy(&bytes);
    import_items(app, u, chat, parse_import(&text)).await
}

async fn dialog_input(app: &App, u: &UserRow, chat: i64, d: Dialog, text: &str) -> Result<()> {
    match d {
        Dialog::RemoveUrl => {
            app.clear_dialog(u.id).await;
            let reply = match first_station_id(text) {
                Some(uuid) if app.db.remove_sub_by_station(u.id, &uuid).await? => {
                    "Подписка удалена."
                }
                Some(_) => "Такой подписки не было.",
                None => "Не удалось распознать ссылку. Используйте /list или /unsubscribe N.",
            };
            send(app, chat, reply, None).await;
        }
        Dialog::Import => {
            app.clear_dialog(u.id).await;
            import_items(app, u, chat, parse_import(text)).await?;
        }
        Dialog::Comment { sub_id, origin } => {
            let n = text.chars().count();
            if n > texts::COMMENT_MAX {
                send(app, chat, &texts::comment_too_long(n), None).await;
                return Ok(());
            }
            app.clear_dialog(u.id).await;
            if !app.db.set_comment(u.id, sub_id, Some(text), now()).await? {
                send(app, chat, texts::SUB_NOT_FOUND, None).await;
                return Ok(());
            }
            send(app, chat, &texts::comment_saved(text), None).await;
            match origin {
                Some((oc, om, CommentOrigin::Card)) => {
                    if let Some((t, k)) = card_view(app, u, sub_id).await? {
                        let _ = app.tg.edit(oc, om, &t, Some(&k)).await;
                    }
                }
                Some((oc, om, CommentOrigin::CommentsList)) => {
                    let subs = app.db.subs_for_user(u.id).await?;
                    let _ = app
                        .tg
                        .edit(
                            oc,
                            om,
                            "Комментарии к станциям:",
                            Some(&kb::comments(&subs)),
                        )
                        .await;
                }
                _ => {}
            }
        }
        Dialog::Quiet => match tz::parse_window(text) {
            Some((a, b)) => {
                app.clear_dialog(u.id).await;
                app.db.set_quiet_window(u.id, a, b).await?;
                let u = app.user_or_default(u.id).await?;
                send_inline(app, chat, &texts::quiet_view(&u), kb::quiet(&u)).await;
            }
            None => send(app, chat, texts::QUIET_BAD, None).await,
        },
        Dialog::Tz => match tz::normalize(text) {
            Some(zone) => {
                app.clear_dialog(u.id).await;
                app.db.set_tz(u.id, &zone).await?;
                let u = app.user_or_default(u.id).await?;
                send_inline(app, chat, &texts::settings(&u, now()), kb::settings(&u)).await;
            }
            None => send(app, chat, texts::TZ_BAD, None).await,
        },
        Dialog::Broadcast => {
            if !app.cfg.is_admin(u.id) {
                app.clear_dialog(u.id).await;
                return Ok(());
            }
            if text.chars().count() > 3500 {
                send(
                    app,
                    chat,
                    "Слишком длинный текст, максимум 3500 символов.",
                    None,
                )
                .await;
                return Ok(());
            }
            let n = app.db.broadcast_targets().await?.len();
            app.set_dialog(u.id, Dialog::BroadcastConfirm(text.to_owned()))
                .await;
            send_inline(
                app,
                chat,
                &texts::broadcast_preview(text, n),
                kb::broadcast_confirm(),
            )
            .await;
        }
        Dialog::BroadcastConfirm(_) => {
            send(
                app,
                chat,
                "Подтвердите или отмените рассылку кнопкой выше. Отмена: /cancel",
                None,
            )
            .await;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Cb {
    ListRefresh,
    ListBack,
    Card(i64),
    History(i64),
    CommentEdit(i64),
    CommentDel(i64),
    CommentDelYes(i64),
    CommentDelNo(i64),
    CommentLater,
    DeleteNow(i64),
    DeleteAsk(i64),
    DeleteYes(i64),
    Share(i64),
    GoneRemove(i64),
    GoneKeep(i64),
    Flag(i64, bool),
    StatusRefresh,
    NotifyRefresh(String),
    Silence(String),
    Role(bool),
    Clear(bool),
    Settings(String),
    Tz(usize),
    TzOther,
    QuietToggle,
    QuietEdit,
    Forget(bool),
    Broadcast(bool),
    Donate,
}

fn parse_cb(data: &str) -> Option<Cb> {
    let parts: Vec<&str> = data.split(':').collect();
    let id = |i: usize| parts.get(i).and_then(|s| s.parse::<i64>().ok());
    Some(match parts.as_slice() {
        ["l" | "list", "r" | "refresh"] => Cb::ListRefresh,
        ["l", "b"] => Cb::ListBack,
        ["c", _] => Cb::Card(id(1)?),
        ["h" | "history", _] => Cb::History(id(1)?),
        ["cm" | "comment", "a" | "e" | "add" | "edit", _] => Cb::CommentEdit(id(2)?),
        ["cm" | "comment", "d" | "del", _] => Cb::CommentDel(id(2)?),
        ["cm" | "comment", "y" | "del_yes", _] => Cb::CommentDelYes(id(2)?),
        ["cm" | "comment", "n" | "del_no", _] => Cb::CommentDelNo(id(2)?),
        ["cm" | "comment", "l" | "later", _] => Cb::CommentLater,
        ["d" | "delete", _] => Cb::DeleteNow(id(1)?),
        ["dq", _] => Cb::DeleteAsk(id(1)?),
        ["dy", _] => Cb::DeleteYes(id(1)?),
        ["sh", _] => Cb::Share(id(1)?),
        ["gd", _] => Cb::GoneRemove(id(1)?),
        ["gk", _] => Cb::GoneKeep(id(1)?),
        ["mf", "b", _] => Cb::Flag(id(2)?, true),
        ["mf", "o", _] => Cb::Flag(id(2)?, false),
        ["st" | "status", "r" | "refresh"] => Cb::StatusRefresh,
        ["nr" | "notify_refresh", uuid] if is_uuid(uuid) => {
            Cb::NotifyRefresh((*uuid).to_ascii_lowercase())
        }
        ["si" | "silence", what] => Cb::Silence((*what).to_owned()),
        ["ro" | "role", "m" | "merchant"] => Cb::Role(true),
        ["ro" | "role", "u" | "user"] => Cb::Role(false),
        ["cl" | "clear", "y" | "yes"] => Cb::Clear(true),
        ["cl" | "clear", "n" | "no"] => Cb::Clear(false),
        ["se", what] => Cb::Settings((*what).to_owned()),
        ["tz", "o"] => Cb::TzOther,
        ["tz", n] => Cb::Tz(n.parse().ok()?),
        ["qh", "t"] => Cb::QuietToggle,
        ["qh", "e"] => Cb::QuietEdit,
        ["fg", "y"] => Cb::Forget(true),
        ["fg", "n"] => Cb::Forget(false),
        ["bc", "y"] => Cb::Broadcast(true),
        ["bc", "n"] => Cb::Broadcast(false),
        ["dn"] => Cb::Donate,
        _ => return None,
    })
}

async fn callback(app: &App, q: CallbackQuery) -> Result<()> {
    if q.from.is_bot {
        return Ok(());
    }
    app.db.touch_user(q.from.id, now()).await?;
    if app.flood_check(q.from.id).await.is_some() {
        app.tg
            .answer_callback(&q.id, Some(texts::TOO_FAST), false)
            .await;
        return Ok(());
    }
    let Some(cb) = q.data.as_deref().and_then(parse_cb) else {
        app.tg
            .answer_callback(&q.id, Some(texts::BAD_BUTTON), false)
            .await;
        return Ok(());
    };
    let Some(msg) = q.message.as_ref().filter(|m| m.chat.kind == "private") else {
        app.tg
            .answer_callback(&q.id, Some(texts::BAD_BUTTON), false)
            .await;
        return Ok(());
    };
    let u = app.user_or_default(q.from.id).await?;
    let (chat, mid) = (msg.chat.id, msg.message_id);
    let answer = match callback_inner(app, &u, chat, mid, msg, cb).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(user_id = u.id, error = format!("{e:#}"), "callback failed");
            Some((texts::INTERNAL_ERROR.to_owned(), true))
        }
    };
    match answer {
        Some((text, alert)) => app.tg.answer_callback(&q.id, Some(&text), alert).await,
        None => app.tg.answer_callback(&q.id, None, false).await,
    }
    Ok(())
}

type Answer = Option<(String, bool)>;

#[allow(clippy::unnecessary_wraps)]
fn toast(s: &str) -> Answer {
    Some((s.to_owned(), false))
}

#[allow(clippy::unnecessary_wraps)]
fn alert(s: &str) -> Answer {
    Some((s.to_owned(), true))
}

async fn callback_inner(
    app: &App,
    u: &UserRow,
    chat: i64,
    mid: i64,
    msg: &Message,
    cb: Cb,
) -> Result<Answer> {
    Ok(match cb {
        Cb::ListRefresh | Cb::ListBack => {
            if cb == Cb::ListRefresh
                && let Err(left) = app.cooldown(u.id, "refresh", REFRESH_COOLDOWN).await
            {
                return Ok(toast(&format!("Подождите {left} с")));
            }
            match list_view(app, u).await? {
                Some((t, k)) => {
                    edit(app, chat, mid, &t, Some(k)).await;
                    if cb == Cb::ListRefresh {
                        toast("Обновлено")
                    } else {
                        None
                    }
                }
                None => {
                    edit(app, chat, mid, "Подписок больше нет.", None).await;
                    None
                }
            }
        }
        Cb::Card(id) => match card_view(app, u, id).await? {
            Some((t, k)) => {
                edit(app, chat, mid, &t, Some(k)).await;
                None
            }
            None => alert(texts::SUB_NOT_FOUND),
        },
        Cb::History(id) => {
            let Some(sub) = app.db.sub(u.id, id).await? else {
                return Ok(alert(texts::SUB_NOT_FOUND));
            };
            let text = history_text(app, u, &sub.station).await?;
            edit(app, chat, mid, &text, Some(kb::back_to_card(id))).await;
            None
        }
        Cb::CommentEdit(id) => {
            let Some(sub) = app.db.sub(u.id, id).await? else {
                return Ok(alert(texts::SUB_NOT_FOUND));
            };
            let origin = match msg.text.as_deref() {
                Some(t) if t.starts_with("Комментарии") => CommentOrigin::CommentsList,
                Some(t) if t.starts_with("Хотите добавить комментарий") => {
                    CommentOrigin::Other
                }
                _ => CommentOrigin::Card,
            };
            app.set_dialog(
                u.id,
                Dialog::Comment {
                    sub_id: id,
                    origin: Some((chat, mid, origin)),
                },
            )
            .await;
            send(
                app,
                chat,
                &texts::comment_prompt(sub.comment.as_deref()),
                None,
            )
            .await;
            None
        }
        Cb::CommentDel(id) => {
            if app.db.sub(u.id, id).await?.is_none() {
                return Ok(alert(texts::SUB_NOT_FOUND));
            }
            edit(
                app,
                chat,
                mid,
                "Удалить комментарий к этой станции?",
                Some(kb::comment_delete_confirm(id)),
            )
            .await;
            None
        }
        Cb::CommentDelYes(id) | Cb::CommentDelNo(id) => {
            let yes = matches!(cb, Cb::CommentDelYes(_));
            if yes && !app.db.set_comment(u.id, id, None, now()).await? {
                return Ok(alert(texts::SUB_NOT_FOUND));
            }
            let subs = app.db.subs_for_user(u.id).await?;
            if subs.is_empty() {
                edit(app, chat, mid, "Подписок нет.", None).await;
            } else {
                edit(
                    app,
                    chat,
                    mid,
                    "Комментарии к станциям:",
                    Some(kb::comments(&subs)),
                )
                .await;
            }
            toast(if yes {
                "🗑 Комментарий удалён."
            } else {
                "Отменено."
            })
        }
        Cb::CommentLater => {
            if matches!(app.dialog(u.id).await, Some(Dialog::Comment { .. })) {
                app.clear_dialog(u.id).await;
            }
            edit(
                app,
                chat,
                mid,
                "Хорошо, комментарий можно добавить позже через /list.",
                None,
            )
            .await;
            None
        }
        Cb::DeleteNow(id) => {
            let deleted = app.db.remove_sub(u.id, id).await?;
            let subs = app.db.subs_for_user(u.id).await?;
            if subs.is_empty() {
                edit(app, chat, mid, "Подписок больше нет.", None).await;
            } else {
                edit(
                    app,
                    chat,
                    mid,
                    "Выберите подписку для удаления:",
                    Some(kb::pick(&subs, "d")),
                )
                .await;
            }
            toast(if deleted {
                "Удалено"
            } else {
                "Подписка уже удалена"
            })
        }
        Cb::DeleteAsk(id) => match app.db.sub(u.id, id).await? {
            Some(s) => {
                edit(
                    app,
                    chat,
                    mid,
                    &texts::delete_confirm(&s.station.name),
                    Some(kb::delete_confirm(id)),
                )
                .await;
                None
            }
            None => alert(texts::SUB_NOT_FOUND),
        },
        Cb::DeleteYes(id) => {
            let deleted = app.db.remove_sub(u.id, id).await?;
            match list_view(app, u).await? {
                Some((t, k)) => edit(app, chat, mid, &t, Some(k)).await,
                None => edit(app, chat, mid, "Подписок больше нет.", None).await,
            }
            toast(if deleted {
                "Удалено"
            } else {
                "Подписка уже удалена"
            })
        }
        Cb::Share(id) => match app.db.sub(u.id, id).await? {
            Some(s) => {
                send(
                    app,
                    chat,
                    &texts::share(&s.station.name, &app.share_link(&s.station.uuid)),
                    None,
                )
                .await;
                None
            }
            None => alert(texts::SUB_NOT_FOUND),
        },
        Cb::GoneRemove(id) | Cb::GoneKeep(id) => {
            let Some(sub) = app.db.sub(u.id, id).await? else {
                edit(app, chat, mid, texts::SUB_NOT_FOUND, None).await;
                return Ok(None);
            };
            let text = if matches!(cb, Cb::GoneRemove(_)) {
                app.db.remove_sub(u.id, id).await?;
                texts::gone_removed(&sub.station.name)
            } else {
                texts::gone_kept(&sub.station.name)
            };
            edit(app, chat, mid, &text, None).await;
            None
        }
        Cb::Flag(id, busy) => {
            if !u.is_merchant {
                return Ok(alert("Эти уведомления доступны только мерчантам (/role)."));
            }
            app.db
                .toggle_sub_flag(
                    u.id,
                    id,
                    if busy {
                        SubFlag::Busy
                    } else {
                        SubFlag::Offline
                    },
                )
                .await?;
            match card_view(app, u, id).await? {
                Some((t, k)) => {
                    edit(app, chat, mid, &t, Some(k)).await;
                    None
                }
                None => alert(texts::SUB_NOT_FOUND),
            }
        }
        Cb::StatusRefresh => {
            if let Err(left) = app.cooldown(u.id, "refresh", REFRESH_COOLDOWN).await {
                return Ok(toast(&format!("Подождите {left} с")));
            }
            match app.status_text(u).await? {
                Some(t) => {
                    edit(app, chat, mid, &t, Some(kb::status())).await;
                    toast("Обновлено")
                }
                None => alert("Подписок нет"),
            }
        }
        Cb::NotifyRefresh(uuid) => notify_refresh(app, u, chat, mid, msg, &uuid).await?,
        Cb::Silence(what) => {
            let t = now();
            let text = match what.as_str() {
                "off" => {
                    app.db.unmute(u.id).await?;
                    let u = app.user_or_default(u.id).await?;
                    let status = app
                        .status_text(&u)
                        .await?
                        .unwrap_or_else(|| texts::NO_SUBS.to_owned());
                    edit(
                        app,
                        chat,
                        mid,
                        &texts::unmuted_report(&status),
                        Some(kb::status()),
                    )
                    .await;
                    return Ok(None);
                }
                "3h" | "8h" | "24h" => {
                    let hours: i64 = what.trim_end_matches('h').parse().unwrap_or(3);
                    app.db.set_mute(u.id, Some(t + hours * 3600)).await?;
                    texts::silence_set(Some(hours))
                }
                "forever" => {
                    app.db.set_mute(u.id, None).await?;
                    texts::silence_set(None)
                }
                _ => return Ok(alert(texts::BAD_BUTTON)),
            };
            edit(app, chat, mid, &text, Some(kb::back_to_settings())).await;
            None
        }
        Cb::Role(merchant) => {
            app.db.set_role(u.id, merchant).await?;
            edit(app, chat, mid, &texts::role_set(merchant), None).await;
            None
        }
        Cb::Clear(true) | Cb::Forget(true) if now() - msg.date > CONFIRM_TTL => {
            edit(app, chat, mid, texts::BUTTON_EXPIRED, None).await;
            None
        }
        Cb::Clear(yes) => {
            if yes {
                let n = app.db.clear_subs(u.id).await?;
                edit(
                    app,
                    chat,
                    mid,
                    &format!("✅ Все подписки удалены ({n} шт.)."),
                    None,
                )
                .await;
                toast(&format!("Удалено подписок: {n}"))
            } else {
                app.tg.delete(chat, mid).await;
                toast("Отменено")
            }
        }
        Cb::Settings(what) => settings_cb(app, u, chat, mid, &what).await?,
        Cb::Tz(i) => {
            let Some((_, zone)) = tz::PRESETS.get(i) else {
                return Ok(alert(texts::BAD_BUTTON));
            };
            app.db.set_tz(u.id, zone).await?;
            let u = app.user_or_default(u.id).await?;
            edit(
                app,
                chat,
                mid,
                &texts::settings(&u, now()),
                Some(kb::settings(&u)),
            )
            .await;
            toast("Часовой пояс сохранён")
        }
        Cb::TzOther => {
            app.set_dialog(u.id, Dialog::Tz).await;
            send(app, chat, texts::TZ_ASK, None).await;
            None
        }
        Cb::QuietToggle => {
            app.db.toggle_user_flag(u.id, UserFlag::Quiet).await?;
            let u = app.user_or_default(u.id).await?;
            edit(app, chat, mid, &texts::quiet_view(&u), Some(kb::quiet(&u))).await;
            None
        }
        Cb::QuietEdit => {
            app.set_dialog(u.id, Dialog::Quiet).await;
            send(app, chat, texts::QUIET_ASK, None).await;
            None
        }
        Cb::Forget(yes) => {
            if yes {
                app.clear_dialog(u.id).await;
                app.db.forget_user(u.id).await?;
                tracing::info!(user_id = u.id, "user data deleted on request");
                edit(app, chat, mid, texts::FORGET_DONE, None).await;
            } else {
                edit(app, chat, mid, texts::CANCELLED, None).await;
            }
            None
        }
        Cb::Broadcast(yes) => {
            let pending = app.dialog(u.id).await;
            app.clear_dialog(u.id).await;
            if !app.cfg.is_admin(u.id) {
                return Ok(alert(texts::ADMIN_ONLY));
            }
            match (yes, pending) {
                (true, Some(Dialog::BroadcastConfirm(text))) => {
                    let targets = app.db.broadcast_targets().await?;
                    let body = html(&text);
                    let msgs = targets
                        .iter()
                        .map(|id| Outgoing::new("broadcast", *id, None, body.as_str(), None))
                        .collect::<Result<Vec<_>>>()?;
                    app.db.enqueue_all(&msgs, now()).await?;
                    app.wake_queue();
                    edit(
                        app,
                        chat,
                        mid,
                        &texts::broadcast_queued(targets.len()),
                        None,
                    )
                    .await;
                }
                (true, _) => {
                    edit(
                        app,
                        chat,
                        mid,
                        "Рассылка устарела, начните заново: /broadcast",
                        None,
                    )
                    .await;
                }
                (false, _) => edit(app, chat, mid, texts::CANCELLED, None).await,
            }
            None
        }
        Cb::Donate => {
            send(
                app,
                chat,
                app.cfg
                    .donate_text
                    .as_deref()
                    .unwrap_or(texts::DONATE_MISSING),
                None,
            )
            .await;
            None
        }
    })
}

async fn settings_cb(app: &App, u: &UserRow, chat: i64, mid: i64, what: &str) -> Result<Answer> {
    match what {
        "m" => {
            edit(
                app,
                chat,
                mid,
                &texts::settings(u, now()),
                Some(kb::settings(u)),
            )
            .await;
        }
        "ro" => {
            edit(
                app,
                chat,
                mid,
                &texts::role_view(u.is_merchant),
                Some(kb::role_change(u.is_merchant)),
            )
            .await;
        }
        "si" => silence_view(app, u, chat, Some(mid)).await,
        "qh" => edit(app, chat, mid, &texts::quiet_view(u), Some(kb::quiet(u))).await,
        "tz" => {
            edit(
                app,
                chat,
                mid,
                &texts::tz_view(&u.tz),
                Some(kb::timezones()),
            )
            .await;
        }
        "or" | "gm" => {
            let flag = if what == "or" {
                UserFlag::OfflineReturn
            } else {
                UserFlag::ShowGame
            };
            app.db.toggle_user_flag(u.id, flag).await?;
            let u = app.user_or_default(u.id).await?;
            edit(
                app,
                chat,
                mid,
                &texts::settings(&u, now()),
                Some(kb::settings(&u)),
            )
            .await;
        }
        "ex" => export(app, u, chat).await?,
        "fg" => send_inline(app, chat, texts::FORGET_ASK, kb::forget_confirm()).await,
        _ => return Ok(alert(texts::BAD_BUTTON)),
    }
    Ok(None)
}

async fn notify_refresh(
    app: &App,
    u: &UserRow,
    chat: i64,
    mid: i64,
    msg: &Message,
    uuid: &str,
) -> Result<Answer> {
    if let Err(left) = app.cooldown(u.id, "refresh", REFRESH_COOLDOWN).await {
        return Ok(toast(&format!("Подождите {left} с")));
    }
    let Some(st) = app.db.station(uuid).await? else {
        return Ok(alert(texts::NO_DATA_YET));
    };
    if st.status == Status::Ready {
        return Ok(toast(texts::STILL_FREE));
    }
    let original = match app.db.sent_text(chat, mid).await? {
        Some(t) => t,
        None => html(msg.text.as_deref().unwrap_or_default()),
    };
    let game = match (&st.product_id, u.show_game) {
        (Some(p), true) => app.titles(std::slice::from_ref(p)).await.remove(p),
        _ => None,
    };
    let text = texts::mark_taken(&original, &texts::taken_suffix(st.status, game.as_deref()));
    edit(
        app,
        chat,
        mid,
        &clip(&text, 4000),
        Some(kb::open_station(uuid)),
    )
    .await;
    Ok(toast("Обновлено"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "05225c83-dd3b-4c12-832b-48227262f960";
    const B: &str = "00009bd4-fbe3-4b1a-b14a-df438f5f0018";

    #[test]
    fn callbacks_new_and_legacy() {
        assert_eq!(parse_cb("l:r"), Some(Cb::ListRefresh));
        assert_eq!(parse_cb("list:refresh"), Some(Cb::ListRefresh));
        assert_eq!(parse_cb("comment:del_yes:7"), Some(Cb::CommentDelYes(7)));
        assert_eq!(parse_cb("delete:3"), Some(Cb::DeleteNow(3)));
        assert_eq!(
            parse_cb(&format!("notify_refresh:{A}")),
            Some(Cb::NotifyRefresh(A.into()))
        );
        assert_eq!(parse_cb("notify_refresh:bad"), None);
        assert_eq!(parse_cb("role:merchant"), Some(Cb::Role(true)));
        assert_eq!(
            parse_cb("silence:forever"),
            Some(Cb::Silence("forever".into()))
        );
        assert_eq!(parse_cb("tz:3"), Some(Cb::Tz(3)));
        assert_eq!(parse_cb("c:x"), None);
        assert_eq!(parse_cb(""), None);
    }

    #[test]
    fn import_lines() {
        let text = format!(
            "https://drova.io/stations/{A} # мой ПК\nhttps://drova.io/stations/{B} https://drova.io/stations/{A} # x\n\njunk"
        );
        let items = parse_import(&text);
        assert_eq!(
            items,
            vec![
                (A.to_owned(), Some("мой ПК".to_owned())),
                (B.to_owned(), None)
            ]
        );
    }
}
