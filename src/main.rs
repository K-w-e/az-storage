mod app;
mod arm;
mod auth;
mod config;
mod model;
mod storage;
mod ui;
mod worker;

use anyhow::Result;
use app::App;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use worker::{Command, Event};

#[tokio::main]
async fn main() -> Result<()> {
    // Config letta prima di entrare in modalità TUI (errori leggibili in shell).
    let config = config::load()?;

    let busy = Arc::new(AtomicUsize::new(0));
    let (tx_cmd, rx_cmd) = mpsc::unbounded_channel::<Command>();
    let (tx_evt, rx_evt) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(worker::run(rx_cmd, tx_evt, busy.clone()));

    // `ratatui::init` abilita raw mode + alternate screen e installa un panic
    // hook che li ripristina (disable_raw_mode + LeaveAlternateScreen).
    let mut terminal = ratatui::init();
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
        ratatui_hook(info);
    }));
    execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;

    let result = run(&mut terminal, App::new(config, busy), tx_cmd, rx_evt).await;

    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    (mut app, init_cmds): (App, Vec<Command>),
    tx: UnboundedSender<Command>,
    mut rx: UnboundedReceiver<Event>,
) -> Result<()> {
    let send = |cmds: Vec<Command>| {
        for c in cmds {
            let _ = tx.send(c);
        }
    };
    send(init_cmds);

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut dirty = true;
    let mut had_status = false;

    let mut mouse_on = app.mouse_enabled;

    while !app.should_quit {
        if mouse_on != app.mouse_enabled {
            mouse_on = app.mouse_enabled;
            if mouse_on {
                execute!(std::io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(std::io::stdout(), DisableMouseCapture)?;
            }
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            dirty = false;
        }
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(TermEvent::Key(k))) if k.kind == KeyEventKind::Press => {
                    send(app.on_key(k));
                    dirty = true;
                }
                Some(Ok(TermEvent::Mouse(m))) => {
                    // Movimenti/drag non richiedono redraw.
                    if !matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) {
                        send(app.on_mouse(m));
                        dirty = true;
                    }
                }
                Some(Ok(TermEvent::Paste(s))) => {
                    app.on_paste(s);
                    dirty = true;
                }
                Some(Ok(TermEvent::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(ev) = rx.recv() => {
                send(app.on_backend(ev));
                // Drena gli eventi in coda per ridurre i redraw.
                while let Ok(ev) = rx.try_recv() {
                    send(app.on_backend(ev));
                }
                dirty = true;
            }
            _ = tick.tick() => {
                let cmds = app.tick();
                if !cmds.is_empty() {
                    send(cmds);
                    dirty = true;
                }
                // Redraw reattivo: solo per spinner o scadenza messaggi di stato.
                let has_status = app.status_text().is_some();
                if app.is_busy() || has_status != had_status {
                    app.spinner = app.spinner.wrapping_add(1);
                    dirty = true;
                }
                had_status = has_status;
            }
        }
    }
    Ok(())
}
