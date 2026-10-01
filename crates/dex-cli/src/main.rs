//! `dex` — the DEX terminal frontend.
//!
//! Presentation only. It connects to a running runtime, sends what the user
//! types, and renders the events that come back. It never reads, writes, searches
//! or executes anything in a repository, and it never starts the runtime: if the
//! socket is missing it says so, because quietly spawning `dexd` would hide the
//! boundary these two repositories exist to make visible.
//!
//! `dex -p "<prompt>"` runs one turn and exits, which is what an acceptance
//! script drives.

mod input;
mod render;

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use dex_client::{Client, ClientError, EventStream, connect};
use dex_protocol::{Event, SessionId, SessionStatus};
use tokio::sync::mpsc;

use input::{Input, Prompt, RawMode, next_key};
use render::Style;

/// The one connection type the frontend uses.
type Session = Client<tokio::net::unix::OwnedWriteHalf>;

#[derive(Parser, Debug)]
#[command(name = "dex", version, about = "Terminal frontend for DEX")]
struct Args {
    /// Run one turn with this prompt and exit.
    #[arg(short, long)]
    prompt: Option<String>,

    /// Open a session in this directory. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Override the socket path from the environment.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Ask the runtime for this model instead of its configured default.
    #[arg(long)]
    model: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let style = Style::detect();

    let socket = expand_home(&args.socket.unwrap_or_else(default_socket));
    let mut client = match connect(&socket).await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("{}", style.red(&e.to_string()));
            std::process::exit(1);
        }
    };

    let working_dir = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let working_dir = working_dir.canonicalize().unwrap_or(working_dir);

    let session = match client
        .create_session(working_dir.display().to_string(), args.model)
        .await
    {
        Ok(session) => session,
        Err(e) => {
            eprintln!("{}", style.red(&format!("could not open a session: {e}")));
            std::process::exit(1);
        }
    };

    match args.prompt {
        Some(prompt) => run_once(&mut client, session, &prompt, &style).await,
        None => repl(&mut client, session, &style).await?,
    }
    Ok(())
}

/// Send one message and print until the turn ends.
async fn run_once(client: &mut Session, session: SessionId, prompt: &str, style: &Style) {
    if let Err(e) = client.send_message(session, prompt).await {
        eprintln!("{}", style.red(&format!("could not send: {e}")));
        return;
    }
    let mut events = client.take_events();
    loop {
        let Some(event) = events.next().await else {
            eprintln!("{}", style.red("the runtime closed the connection"));
            return;
        };
        let finished = is_finished(&event);
        emit(style, &event);
        if finished {
            return;
        }
    }
}

/// Only a terminal status ends a turn.
fn is_finished(event: &Event) -> bool {
    matches!(
        event,
        Event::SessionFinished {
            status: SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
        }
    )
}

fn emit(style: &Style, event: &Event) {
    if let Some(line) = render::render(style, event) {
        print!("{line}");
        let _ = std::io::stdout().flush();
    }
}

/// The interactive loop.
async fn repl(client: &mut Session, session: SessionId, style: &Style) -> anyhow::Result<()> {
    // Restored on drop as well as explicitly, so a panic mid-turn still leaves
    // a usable terminal behind.
    let _raw = RawMode::enter()?;
    println!(
        "{}",
        style.dim("Type a task, or /quit to leave. Ctrl-C cancels a running turn.")
    );

    let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();
    let (interrupts_tx, mut interrupts_rx) = mpsc::unbounded_channel::<()>();
    // One event stream for the whole session, taken here rather than borrowed
    // by a task per turn.
    let mut events: EventStream = client.take_events();

    // The keyboard is its own task: the loop has to stay responsive to Ctrl-C
    // while a turn streams, and must not block waiting for a line.
    tokio::spawn(async move {
        let mut prompt = Prompt::new();
        loop {
            let Some(key) = next_key().await else { return };
            match prompt.apply(key) {
                Some(Input::Line(line)) => {
                    if lines_tx.send(line).is_err() {
                        return;
                    }
                }
                Some(Input::Interrupt) => {
                    if interrupts_tx.send(()).is_err() {
                        return;
                    }
                }
                Some(Input::Eof) => return,
                None => {}
            }
            redraw(&prompt);
        }
    });

    loop {
        let line = tokio::select! {
            line = lines_rx.recv() => match line {
                Some(line) => line,
                // The keyboard task ended, so the terminal is gone: leave.
                None => break,
            },
            // Ctrl-C at the prompt, with no turn to cancel.
            _ = interrupts_rx.recv() => break,
        };

        if line == "/quit" || line == "/exit" {
            break;
        }
        redraw(&Prompt::new());
        println!();

        if let Err(e) = client.send_message(session, &line).await {
            println!("{}", style.red(&format!("could not send: {e}")));
            continue;
        }
        if let Err(e) = drive_turn(client, session, style, &mut events, &mut interrupts_rx).await {
            println!("{}", style.red(&format!("{e}")));
        }
    }

    let _ = client.close(session).await;
    let _ = RawMode::restore();
    println!("{}", style.dim("bye"));
    Ok(())
}

/// Run a turn while staying responsive to Ctrl-C.
///
/// Events and interrupts are selected together, so a cancel reaches the runtime
/// while output is still streaming. Events are forwarded by a task because the
/// client's own channel is also where acknowledgements land.
async fn drive_turn(
    client: &mut Session,
    session: SessionId,
    style: &Style,
    events: &mut EventStream,
    interrupts: &mut mpsc::UnboundedReceiver<()>,
) -> Result<(), ClientError> {
    let mut cancelling = false;
    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else { return Ok(()) };
                let finished = is_finished(&event);
                emit(style, &event);
                if finished {
                    return Ok(());
                }
            }
            interrupt = interrupts.recv() => {
                if interrupt.is_none() {
                    // The keyboard is gone. Finish the turn rather than
                    // leaving a program running with nobody watching.
                    return wait_for_turn(events, style).await;
                }
                if cancelling {
                    println!("\n{}", style.dim("already cancelling"));
                    continue;
                }
                cancelling = true;
                println!("\n{} {}", style.yellow("cancelling"), style.dim("..."));
                let _ = std::io::stdout().flush();
                if let Err(e) = client.cancel(session).await {
                    println!("{}", style.red(&format!("could not cancel: {e}")));
                    cancelling = false;
                }
            }
        }
    }
}

/// Finish printing a turn with no way to interrupt it.
async fn wait_for_turn(events: &mut EventStream, style: &Style) -> Result<(), ClientError> {
    while let Some(event) = events.next().await {
        let finished = is_finished(&event);
        emit(style, &event);
        if finished {
            return Ok(());
        }
    }
    Ok(())
}

/// Repaint the prompt line in place.
fn redraw(prompt: &Prompt) {
    // Plain here on purpose: this runs on every keystroke, and escape codes
    // smear across a short terminal.
    print!("\r\u{203a} {}\u{2588}", prompt.buffer());
    let _ = std::io::stdout().flush();
}

/// Where the runtime is expected to be listening.
fn default_socket() -> PathBuf {
    std::env::var_os("DEX_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~/.dex/dex.sock"))
}

fn expand_home(path: &std::path::Path) -> PathBuf {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_terminal_status_ends_a_turn() {
        for status in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
        ] {
            assert!(
                is_finished(&Event::SessionFinished { status }),
                "{status:?} should end the turn"
            );
        }
        for status in [
            SessionStatus::Created,
            SessionStatus::Running,
            SessionStatus::Waiting,
        ] {
            assert!(
                !is_finished(&Event::SessionFinished { status }),
                "{status:?} should not end the turn"
            );
        }
    }

    #[test]
    fn an_answer_alone_does_not_end_a_turn() {
        // The session status is what ends it; an answer mid-turn is data.
        assert!(!is_finished(&Event::Answer {
            text: "x".into()
        }));
    }

    #[test]
    fn the_socket_path_is_resolved_against_the_home_directory() {
        if std::env::var_os("HOME").is_some() {
            let resolved = expand_home(&PathBuf::from("~/.dex/dex.sock"));
            let text = resolved.display().to_string();
            assert!(text.ends_with(".dex/dex.sock"), "got {text}");
            assert!(!text.contains('~'), "the tilde should be expanded: {text}");
        }
    }
}