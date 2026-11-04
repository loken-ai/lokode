//! Piloting the sessions of several lokode servers from one place.
//!
//! A server started with `lokode serve --share` announces itself to the machines of a group and
//! accepts their dashboards over an encrypted channel.
//! A group is a name and a secret, written under `[dashboard]` in the user's configuration.

pub mod actions;
pub mod client;
pub mod discovery;
pub mod state;
pub mod view;
pub mod wire;

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;

/// The addresses in `seeds`, which are written `host:port`.
/// A seed that does not resolve is left out and named in the second value.
pub fn resolve_seeds(seeds: &[String]) -> (Vec<SocketAddr>, Vec<String>) {
    let mut found = Vec::new();
    let mut lost = Vec::new();
    for seed in seeds {
        match seed.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => found.push(addr),
            None => lost.push(seed.clone()),
        }
    }
    (found, lost)
}

/// What opens a session of a server in this terminal, given the address of a bridge to it.
pub type Opener =
    Box<dyn Fn(String, String) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

/// Show the sessions of the group in this terminal until `q` is pressed.
///
/// `open` shows one session whole, as `lokode attach` does.
pub async fn run(group: wire::Group, seeds: Vec<SocketAddr>, open: Opener) -> Result<()> {
    use std::sync::{Arc, Mutex};
    let (fleet, task) = client::follow(group, seeds);
    crate::ui::term::install_panic_hook();
    let mut terminal_guard = Some(crate::ui::term::TerminalGuard::new()?);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut selected = 0usize;
    let note: Arc<Mutex<String>> = Default::default();
    let counter = std::sync::atomic::AtomicU64::new(1);
    // The message being typed, when one is.
    let mut typing: Option<String> = None;
    loop {
        let items = match fleet.lock() {
            Ok(fleet) => view::lines(&fleet, Instant::now()),
            Err(_) => Vec::new(),
        };
        selected = selected.min(items.len().saturating_sub(1));
        let shown = match (&typing, items.get(selected)) {
            (Some(text), _) => format!("message: {text}▏  (enter sends, esc cancels)"),
            (None, Some(item)) if item.ask.is_some() => {
                let always = item.ask.as_ref().map(|a| a.1.as_str()).unwrap_or_default();
                format!("y once  a {always}  n reject")
            }
            _ => note.lock().map(|n| n.clone()).unwrap_or_default(),
        };
        terminal.draw(|frame| view::draw(frame, &items, selected, &shown))?;
        tokio::select! {
            _ = tick.tick() => {}
            event = keys.next() => {
                let Some(Ok(Event::Key(key))) = event else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let item = items.get(selected).cloned();
                let remote = item.as_ref().and_then(|i| {
                    fleet.lock().ok().and_then(|f| f.servers.get(&i.node).and_then(|e| e.link.clone()))
                });
                if let Some(text) = typing.as_mut() {
                    match key.code {
                        KeyCode::Esc => typing = None,
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        KeyCode::Char(c) => text.push(c),
                        KeyCode::Enter => {
                            let text = typing.take().unwrap_or_default();
                            if let (Some(remote), Some(item)) = (remote, item) {
                                if let (Some(session), false) = (item.session, text.trim().is_empty()) {
                                    let id = format!("dash-{}", counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                                    send(&remote, actions::prompt(&session, &text, &id), "message sent", &note);
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => selected += 1,
                    KeyCode::Char(c @ ('y' | 'a' | 'n')) => {
                        if let (Some(remote), Some(Some((ask, _)))) = (remote, item.map(|i| i.ask)) {
                            let option = match c {
                                'y' => "once",
                                'a' => "always",
                                _ => "no",
                            };
                            send(&remote, actions::answer(ask, option), "answered", &note);
                        }
                    }
                    KeyCode::Char('x') => {
                        if let (Some(remote), Some(session)) = (remote, item.and_then(|i| i.session)) {
                            send(&remote, actions::cancel(&session), "stop asked", &note);
                        }
                    }
                    KeyCode::Char('m') => {
                        if item.as_ref().is_some_and(|i| i.session.is_some()) && remote.is_some() {
                            typing = Some(String::new());
                        }
                    }
                    KeyCode::Enter => {
                        if let (Some(remote), Some(session)) = (remote, item.and_then(|i| i.session)) {
                            let stop = tokio_util::sync::CancellationToken::new();
                            match crate::serve::bridge::start(remote, stop.clone()).await {
                                Ok(url) => {
                                    // The screen of the session takes the terminal, then gives it back.
                                    drop(terminal_guard.take());
                                    let shown = open(url, session).await;
                                    stop.cancel();
                                    terminal_guard = Some(crate::ui::term::TerminalGuard::new()?);
                                    terminal.clear()?;
                                    if let Err(e) = shown {
                                        if let Ok(mut n) = note.lock() {
                                            *n = format!("the session could not be shown: {e:#}");
                                        }
                                    }
                                }
                                Err(e) => {
                                    if let Ok(mut n) = note.lock() {
                                        *n = format!("the session could not be reached: {e:#}");
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    task.abort();
    drop(terminal_guard);
    Ok(())
}

/// Post `message` to `remote` without holding the screen up, and say what came of it in `note`.
fn send(
    remote: &std::sync::Arc<client::Remote>,
    message: serde_json::Value,
    done: &'static str,
    note: &std::sync::Arc<std::sync::Mutex<String>>,
) {
    let (remote, note) = (remote.clone(), note.clone());
    tokio::spawn(async move {
        let said = match remote.call(message).await {
            Ok(_) => done.to_string(),
            Err(e) => format!("it failed: {e}"),
        };
        if let Ok(mut n) = note.lock() {
            *n = said;
        }
    });
}
