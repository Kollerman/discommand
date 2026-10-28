mod api;
mod app;
mod cal;
mod gateway;
mod model;
mod pics;
mod qr;
mod ra;
mod secret;
mod state;
mod text;
mod todo;
mod ui;
mod view;
mod wa;

use std::io;
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::api::Api;
use crate::app::{App, AppEvent};

const USAGE: &str = "discommand — lightweight terminal client for Discord and WhatsApp

usage: discommand [--wa-phone NUMBER] [--discord-token] [--logout] [--help] [--version]

Both accounts are required. The Discord token is read from $DISCORD_TOKEN,
then from the saved token (encrypted with DPAPI on Windows), otherwise you
log in once by scanning a QR code with the Discord app.
WhatsApp is linked once by scanning a QR code (Linked devices).

  --discord-token    paste a Discord token instead of the QR-code login
  --wa-phone NUMBER  link WhatsApp with a pairing code instead of a QR code
                     (international format, e.g. +491701234567)
  --logout           delete the saved Discord token and exit
                     (unlink WhatsApp with :wa logout inside the app)";

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
        Ok(p) => println!("Token saved (encrypted on Windows) to {}", p.display()),
        Err(e) => eprintln!("warning: could not save token: {e}"),
    }
    Ok(token)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut wa_phone = None;
    let mut paste_token = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return println!("{USAGE}"),
            "-V" | "--version" => return println!("discommand {}", env!("CARGO_PKG_VERSION")),
            "--logout" => {
                secret::remove_token();
                return println!("Saved Discord token removed.");
            }
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
    if let Err(e) = rt.block_on(run(wa_phone, paste_token)) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(wa_phone: Option<String>, paste_token: bool) -> Result<(), String> {
    let token = obtain_token(paste_token).await?;
    let api = Api::new(&token)?;
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let (gw_tx, gw_rx) = unbounded_channel::<String>();
    // Discord connects while WhatsApp starts (or is being linked); its events queue up.
    tokio::spawn(gateway::run(token, tx.clone(), gw_rx));

    // Before the helper opens its database: encrypt data folders at rest.
    secret::protect_dirs();
    let mut sidecar = wa::spawn()?;
    let replay = wa::login(&mut sidecar, wa_phone.as_deref()).await?;
    let wa::Sidecar { mut events, cmds, mut child } = sidecar;
    let wa_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if wa_tx.send(AppEvent::Wa(ev)).is_err() {
                return;
            }
        }
    });

    let mut app = App::new(api, tx, gw_tx, cmds);
    app.cal_sync();
    app.todo_sync();
    for ev in replay {
        app.on_event(AppEvent::Wa(ev));
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
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    if logout {
        secret::remove_token();
        println!("Logged out; saved token removed.");
    }
    res.map_err(|e| e.to_string())
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
