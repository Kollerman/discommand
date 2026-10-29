mod api;
mod app;
mod cal;
mod gateway;
#[cfg(feature = "matrix")]
mod matrix;
mod model;
mod pics;
mod qr;
#[cfg(feature = "discord")]
mod ra;
mod secret;
mod state;
mod text;
mod todo;
mod twos;
mod ui;
mod update;
mod view;
mod wa;

use std::io;
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::api::Api;
use crate::app::{App, AppEvent, Service};

#[cfg(not(any(
    feature = "discord",
    feature = "whatsapp",
    feature = "matrix",
    feature = "calendar",
    feature = "todo",
    feature = "twos"
)))]
compile_error!("build with at least one integration: discord, whatsapp, matrix, calendar, todo, twos");

const USAGE: &str = "discommand — lightweight terminal client for Discord, WhatsApp, Matrix,
Google Calendar, Microsoft To Do and Twos (whichever are built in, see --version)

usage: discommand [--wa-phone NUMBER] [--discord-token] [--logout] [--logout-a] [--help] [--version]

The accounts of the chat services built in are required. The Discord token is read from $DISCORD_TOKEN,
then from the saved token (encrypted with DPAPI on Windows), otherwise you
log in once by scanning a QR code with the Discord app.
WhatsApp is linked once by scanning a QR code (Linked devices).

  --discord-token    paste a Discord token instead of the QR-code login
  --wa-phone NUMBER  link WhatsApp with a pairing code instead of a QR code
                     (international format, e.g. +491701234567)
  --logout           delete the saved Discord token and exit
                     (unlink WhatsApp with :wa logout inside the app)
  --logout-a         log out of everything: Discord, WhatsApp (unlinked),
                     Matrix, Google, Microsoft and Twos; also deletes the
                     WhatsApp chat history, the Matrix store and cached media.
                     Close discommand first.";

fn clean(t: &str) -> String {
    t.trim().trim_matches('"').trim().to_string()
}

/// The Discord token: from $DISCORD_TOKEN, the saved one, a QR-code login
/// (default), or pasted by hand (`--discord-token`, or if QR login fails).
async fn obtain_token(paste: bool) -> Result<String, String> {
    if let Ok(t) = std::env::var("DISCORD_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(clean(&t));
        }
    }
    if let Some(t) = secret::load_token() {
        if !t.trim().is_empty() {
            return Ok(clean(&t));
        }
    }
    // Only called with Discord built in; the QR login (and RSA) are part of that feature.
    #[cfg(not(feature = "discord"))]
    let _ = paste;
    #[cfg(feature = "discord")]
    if !paste {
        match ra::login().await {
            Ok(token) => return finish_login(clean(&token)).await,
            Err(e) => println!("\nQR login didn't work ({e}). You can paste a token instead.\n"),
        }
    }
    println!("Get the token from the web client: DevTools (F12) → Network → any request to");
    println!("discord.com/api → request header \"Authorization\".");
    println!("Never share this token: it gives full access to your account.\n");
    let token = clean(&rpassword::prompt_password("Token (input hidden): ").map_err(|e| e.to_string())?);
    if token.is_empty() {
        return Err("no token given".into());
    }
    finish_login(token).await
}

/// Checks a new token and saves it.
async fn finish_login(token: String) -> Result<String, String> {
    let api = Api::new(&token)?;
    let me = api.me().await?;
    println!("Logged in as {}.", me.name());
    match secret::save_token(&token) {
        Ok(place) => println!("Token saved to {place}"),
        Err(e) => eprintln!("warning: could not save token: {e}"),
    }
    Ok(token)
}

/// `--logout-a`: signs out of every service (on the service's side too
/// where possible) and deletes all saved logins and local chat data.
async fn logout_all() {
    if let Some(token) = secret::load_token() {
        let result = match Api::new(&token) {
            Ok(api) => api.logout().await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => println!("Discord: logged out"),
            Err(e) => println!("Discord: saved token removed (Discord didn't confirm the logout: {e})"),
        }
    }
    secret::remove_token();

    if let Some(c) = cal::Creds::load() {
        match (c.refresh_token.is_empty(), cal::revoke(&c.refresh_token).await) {
            (false, Ok(())) => println!("Google Calendar: signed out, access revoked"),
            (false, Err(e)) => println!("Google Calendar: sign-in removed (Google didn't confirm the revocation: {e})"),
            (true, _) => println!("Google Calendar: app setup removed"),
        }
        cal::Creds::remove();
    }
    if todo::Creds::load().is_some() {
        todo::Creds::remove();
        println!("Microsoft To Do: sign-in removed (the app's access can be revoked at account.microsoft.com → Privacy)");
    }
    if twos::load_key().is_some() {
        twos::remove_key();
        println!("Twos: API key removed (revoke it in Twos → Settings → API Keys)");
    }

    #[cfg(feature = "matrix")]
    if matrix::has_session() {
        match matrix::logout().await {
            Ok(()) => println!("Matrix: logged out"),
            Err(e) => println!("Matrix: session deleted (the server didn't confirm the logout: {e})"),
        }
    }

    if let Some(db) = secret::data_dir().map(|d| d.join("whatsapp.db")).filter(|db| db.exists()) {
        match wa::unlink().await {
            Ok(true) => println!("WhatsApp: unlinked"),
            Ok(false) => println!("WhatsApp: wasn't linked"),
            Err(e) => println!("WhatsApp: couldn't unlink ({e}); remove it under Linked devices on your phone"),
        }
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = db.clone().into_os_string();
            name.push(suffix);
            secret::wipe(std::path::Path::new(&name));
        }
        println!("WhatsApp: local session and chat history deleted");
    }
    let _ = std::fs::remove_dir_all(secret::cache_dir());
    println!("Cached pictures and downloads deleted. Logged out of everything.");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut wa_phone = None;
    let mut paste_token = false;
    let mut logout_everything = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return println!("{USAGE}"),
            "-V" | "--version" => {
                let built: Vec<_> = Service::ALL.iter().map(|s| s.name()).collect();
                return println!("discommand {} ({})", env!("CARGO_PKG_VERSION"), built.join(", "));
            }
            "--logout" => {
                secret::remove_token();
                return println!("Saved Discord token removed.");
            }
            "--logout-a" | "--logout-all" => logout_everything = true,
            "--discord-token" => paste_token = true,
            "--wa-phone" => match args.next() {
                Some(n) => wa_phone = Some(n),
                None => {
                    eprintln!("--wa-phone needs a number\n\n{USAGE}");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("unknown argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    // A single-threaded runtime: the app is I/O bound and this keeps CPU wakeups minimal.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if logout_everything {
        return rt.block_on(logout_all());
    }
    if let Err(e) = rt.block_on(run(wa_phone, paste_token)) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(wa_phone: Option<String>, paste_token: bool) -> Result<(), String> {
    // Matrix's HTTP client uses rustls and needs its crypto backend chosen.
    #[cfg(feature = "matrix")]
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let (gw_tx, gw_rx) = unbounded_channel::<String>();
    // Without Discord built in, the API client has no token and is never used.
    let api = if Service::Discord.built() {
        let token = obtain_token(paste_token).await?;
        let api = Api::new(&token)?;
        // Discord connects while WhatsApp starts (or is being linked); its events queue up.
        tokio::spawn(gateway::run(token, tx.clone(), gw_rx));
        api
    } else {
        Api::new("")?
    };

    // Before the helper opens its database: encrypt data folders at rest.
    secret::protect_dirs();
    let (cmds, mut child, replay) = if Service::WhatsApp.built() {
        let mut sidecar = wa::spawn()?;
        let replay = wa::login(&mut sidecar, wa_phone.as_deref()).await?;
        let wa::Sidecar { mut events, cmds, child } = sidecar;
        let wa_tx = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if wa_tx.send(AppEvent::Wa(Service::WhatsApp, ev)).is_err() {
                    return;
                }
            }
        });
        (cmds, Some(child), replay)
    } else {
        // Commands to WhatsApp go nowhere.
        (unbounded_channel().0, None, Vec::new())
    };

    let mx_cmds = if Service::Matrix.built() { start_matrix(&tx).await? } else { unbounded_channel().0 };
    if update::enabled() {
        let tx = tx.clone();
        tokio::spawn(update::watch(move |tag| {
            let _ = tx.send(AppEvent::Update(tag));
        }));
    }

    let mut app = App::new(api, tx, gw_tx, cmds, mx_cmds);
    app.cal_sync();
    app.todo_sync(Service::Tasks);
    app.todo_sync(Service::Twos);
    for ev in replay {
        app.on_event(AppEvent::Wa(Service::WhatsApp, ev));
    }
    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let res = event_loop(&mut terminal, &mut app, &mut rx).await;
    let logout = app.logout;
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();

    app.flush_ack().await;
    // Closing the helper's stdin lets it disconnect cleanly; kill it if it hangs.
    drop(app);
    if let Some(child) = child.as_mut() {
        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
    if logout {
        secret::remove_token();
        println!("Logged out; saved token removed.");
    }
    res.map_err(|e| e.to_string())
}

/// Logs in to Matrix (in the terminal, before the UI) and starts syncing.
#[cfg(feature = "matrix")]
async fn start_matrix(tx: &UnboundedSender<AppEvent>) -> Result<UnboundedSender<String>, String> {
    let client = matrix::login().await?;
    let (mut events, cmds) = matrix::start(client);
    let tx = tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if tx.send(AppEvent::Wa(Service::Matrix, ev)).is_err() {
                return;
            }
        }
    });
    Ok(cmds)
}

#[cfg(not(feature = "matrix"))]
async fn start_matrix(_: &UnboundedSender<AppEvent>) -> Result<UnboundedSender<String>, String> {
    Ok(unbounded_channel().0)
}

async fn sleep_until(at: Option<std::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t.into()).await,
        None => std::future::pending().await,
    }
}

/// Reads terminal input on a dedicated thread that blocks in `read()`, so
/// input never waits on the async runtime (and costs nothing while idle).
/// Events already queued are sent as one batch: on Windows a paste arrives as
/// a burst of key presses, and an Enter inside a burst means "newline".
fn spawn_input(tx: UnboundedSender<io::Result<Vec<Event>>>) {
    std::thread::spawn(move || loop {
        let batch = event::read().and_then(|first| {
            let mut batch = vec![first];
            while batch.len() < 8192 && event::poll(Duration::ZERO)? {
                batch.push(event::read()?);
            }
            Ok(batch)
        });
        let failed = batch.is_err();
        if tx.send(batch).is_err() || failed {
            return;
        }
    });
}

async fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, rx: &mut UnboundedReceiver<AppEvent>) -> io::Result<()> {
    let (input_tx, mut input) = unbounded_channel();
    spawn_input(input_tx);
    loop {
        // Redraw only when something changed. The only timer is for calendar
        // reminders/polling, and it sleeps until exactly when it's needed.
        if app.dirty {
            if app.tree_dirty {
                app.rebuild_tree();
            }
            app.sync_page();
            terminal.draw(|f| ui::draw(f, app))?;
            app.dirty = false;
            app.load_pics();
        }
        let wake = app.next_wake();
        tokio::select! {
            biased;
            batch = input.recv() => {
                let Some(batch) = batch else { return Ok(()) };
                let mut batch = batch?;
                while let Ok(more) = input.try_recv() {
                    batch.extend(more?);
                }
                let presses = batch.iter().filter(|e| matches!(e, Event::Key(k) if k.kind == KeyEventKind::Press)).count();
                let paste = presses > 2;
                for e in batch {
                    app.on_term(e, paste);
                }
            }
            Some(ev) = rx.recv() => {
                app.on_event(ev);
                while let Ok(ev) = rx.try_recv() {
                    app.on_event(ev);
                }
            }
            _ = sleep_until(wake) => app.on_tick(),
        }
        if app.quit {
            return Ok(());
        }
    }
}
