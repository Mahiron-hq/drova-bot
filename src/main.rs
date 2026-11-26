mod app;
mod config;
mod db;
mod drova;
mod handlers;
mod history;
mod jobs;
mod keyboards;
mod legacy;
mod monitor;
mod sender;
mod systemd;
mod telegram;
mod texts;
mod tz;
mod util;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::app::App;
use crate::config::Config;
use crate::telegram::{BotCommand, Telegram, TgError};
use crate::util::now;

const USAGE: &str = "drova-bot - Telegram bot that watches Drova stations

Usage:
  drova-bot [run]                         start the bot (settings from environment or .env)
  drova-bot migrate-legacy <old> <new>    convert the database of the old Python bot
  drova-bot check-token                   check BOT_TOKEN and print the bot username
  drova-bot --version | --help
";

const USER_COMMANDS: &[BotCommand] = &[
    BotCommand {
        command: "list",
        description: "Мои подписки",
    },
    BotCommand {
        command: "status",
        description: "Статус станций",
    },
    BotCommand {
        command: "history",
        description: "История и статистика станции",
    },
    BotCommand {
        command: "settings",
        description: "Роль, тишина, тихие часы, часовой пояс",
    },
    BotCommand {
        command: "help",
        description: "Справка",
    },
    BotCommand {
        command: "about",
        description: "О боте и исходный код",
    },
    BotCommand {
        command: "privacy",
        description: "Конфиденциальность",
    },
    BotCommand {
        command: "donate",
        description: "Поддержать проект",
    },
    BotCommand {
        command: "start",
        description: "Начало работы",
    },
];

const ADMIN_COMMANDS: &[BotCommand] = &[
    BotCommand {
        command: "admin",
        description: "Админ: статистика и ошибки",
    },
    BotCommand {
        command: "broadcast",
        description: "Админ: рассылка всем",
    },
];

fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn main() -> std::process::ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "cannot start runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(dispatch(&args));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "fatal error");
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn dispatch(args: &[String]) -> Result<()> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["run"] => run().await,
        ["--version" | "-V" | "version"] => {
            out(&format!("drova-bot {}", texts::VERSION));
            Ok(())
        }
        ["--help" | "-h" | "help"] => {
            out(USAGE);
            Ok(())
        }
        ["migrate-legacy", src, dst] => {
            init_logging(tracing::Level::INFO);
            let report = legacy::migrate(&PathBuf::from(src), &PathBuf::from(dst)).await?;
            out(&format!(
                "migrated: users={} stations={} subscriptions={} comments={} transitions={} campaigns={} skipped={}",
                report.users,
                report.stations,
                report.subscriptions,
                report.comments,
                report.transitions,
                report.campaigns,
                report.skipped_subscriptions
            ));
            Ok(())
        }
        ["check-token"] => {
            load_env();
            let cfg = Config::from_env()?;
            let tg = Telegram::new(&cfg.bot_token, None)?;
            let me = tg.get_me().await.map_err(|e| anyhow::anyhow!("{e}"))?;
            out(&format!(
                "bot_id={}\nusername=@{}",
                me.id,
                me.username.unwrap_or_default()
            ));
            Ok(())
        }
        _ => bail!("unknown arguments\n\n{USAGE}"),
    }
}

fn load_env() {
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found()
    {
        let _ = writeln!(std::io::stderr(), "warning: cannot read .env: {e}");
    }
}

fn init_logging(level: tracing::Level) {
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(!journald);
    if journald {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

async fn wait_for_me(tg: &Telegram, shutdown: &mut watch::Receiver<bool>) -> Result<String> {
    let mut delay = 5;
    loop {
        match tg.get_me().await {
            Ok(me) => return me.username.context("bot has no username"),
            Err(TgError::Api(401 | 404, _)) => bail!("BOT_TOKEN was rejected by Telegram"),
            Err(e) => {
                tracing::warn!(error = %e, retry_in = delay, "Telegram is not reachable yet");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(delay)) => {}
                    _ = shutdown.changed() => bail!("stopped before start"),
                }
                delay = (delay * 2).min(60);
            }
        }
    }
}

async fn register_commands(app: &App) {
    if let Err(e) = app.tg.set_commands(USER_COMMANDS, None).await {
        tracing::warn!(error = %e, "cannot set bot commands");
    }
    for id in &app.cfg.admin_ids {
        let mut all: Vec<BotCommand> = USER_COMMANDS.to_vec();
        all.extend_from_slice(ADMIN_COMMANDS);
        match app.tg.set_commands(&all, Some(*id)).await {
            Ok(()) | Err(TgError::Suppressed) => {}
            Err(e) => tracing::warn!(admin = id, error = %e, "cannot set admin commands"),
        }
    }
}

fn spawn_signals(tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                let _ = tokio::signal::ctrl_c().await;
                let _ = tx.send(true);
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::info!("shutdown requested");
        let _ = tx.send(true);
    });
}

fn spawn_watchdog(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    let Some(every) = systemd::watchdog_interval() else {
        return;
    };
    let cycle_limit = (app.cfg.poll_interval.as_secs() as i64 * 12).max(900);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                _ = shutdown.changed() => return,
            }
            let t = now();
            let updates_ok = t - app.health.last_updates.load(Ordering::Relaxed) < 300;
            let cycle_ok = t - app
                .health
                .last_cycle
                .load(Ordering::Relaxed)
                .max(app.health.started.load(Ordering::Relaxed))
                < cycle_limit;
            if updates_ok && cycle_ok {
                systemd::notify("WATCHDOG=1");
            } else {
                tracing::warn!(updates_ok, cycle_ok, "watchdog not fed");
            }
        }
    });
}

async fn run() -> Result<()> {
    load_env();
    let cfg = Config::from_env()?;
    init_logging(cfg.log_level);
    tracing::info!(version = texts::VERSION, config = ?cfg, "starting");
    if cfg.allowed_chat_ids.is_some() {
        tracing::warn!("test mode: messages go only to ALLOWED_CHAT_IDS");
    }
    let (tx, mut shutdown) = watch::channel(false);
    spawn_signals(tx.clone());

    let db = db::Db::open(&cfg.database_path).await?;
    let tg = Telegram::new(&cfg.bot_token, cfg.allowed_chat_ids.clone())?;
    let drova = drova::DrovaClient::new(cfg.http_timeout)?;
    let username = match wait_for_me(&tg, &mut shutdown).await {
        Ok(u) => u,
        Err(e) => {
            db.close().await;
            return Err(e);
        }
    };
    tracing::info!(bot = %username, "connected to Telegram");
    let app = Arc::new(App::new(cfg, db, tg, drova, username));
    app.health.last_updates.store(now(), Ordering::Relaxed);
    register_commands(&app).await;

    let mut workers = JoinSet::new();
    workers.spawn(monitor::Monitor::new(app.clone()).run(shutdown.clone()));
    workers.spawn(sender::run(app.clone(), shutdown.clone()));
    workers.spawn(jobs::run(app.clone(), shutdown.clone()));
    spawn_watchdog(app.clone(), shutdown.clone());
    systemd::notify("READY=1");

    let mut handlers = JoinSet::new();
    let mut offset = 0_i64;
    let mut delay = 1;
    loop {
        let updates = tokio::select! {
            r = app.tg.get_updates(offset, 50) => r,
            _ = shutdown.changed() => break,
        };
        app.health.last_updates.store(now(), Ordering::Relaxed);
        match updates {
            Ok(list) => {
                delay = 1;
                for upd in list {
                    offset = offset.max(upd.update_id + 1);
                    handlers.spawn(handlers::handle(app.clone(), upd));
                }
            }
            Err(TgError::Api(401, _)) => {
                tracing::error!("BOT_TOKEN was revoked");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_in = delay, "getUpdates failed");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(delay)) => {}
                    _ = shutdown.changed() => break,
                }
                delay = (delay * 2).min(30);
            }
        }
        while handlers.try_join_next().is_some() {}
    }

    systemd::notify("STOPPING=1");
    tracing::info!("stopping");
    let _ = tx.send(true);
    if offset > 0 {
        let _ = app.tg.get_updates(offset, 0).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        while handlers.join_next().await.is_some() {}
    })
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        while workers.join_next().await.is_some() {}
    })
    .await;
    app.db.close().await;
    tracing::info!("stopped");
    Ok(())
}
