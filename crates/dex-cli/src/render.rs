//! Rendering events as terminal output.
//!
//! The frontend's whole job. Every line here comes from an event the runtime
//! emitted; nothing is inferred and nothing is fetched. If it is not on screen,
//! the runtime did not say it.
//!
//! Output is line-based rather than a full-screen display. A turn can produce a
//! lot of text, and a scrollback the user can search and copy is worth more than
//! a pane that repaints. Streaming text is printed as it arrives, so the model
//! looks alive without a redraw loop.

use dex_protocol::{
    Event, FileChange, OutputStream, SessionStatus,
};

/// ANSI styling, off when the output is not a terminal or `NO_COLOR` is set.
#[derive(Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn detect() -> Self {
        let disabled = std::env::var_os("NO_COLOR").is_some()
            || !matches!(
                std::env::var("TERM").as_deref().ok(),
                None | Some("dumb") | Some("")
            );
        Self {
            enabled: !disabled,
        }
    }

    /// A style that emits no escapes, for writing somewhere that is not a
    /// terminal. The renderer uses this whenever NO_COLOR is set.
    #[allow(dead_code)]
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_string()
        }
    }

    pub fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
    pub fn bold(&self, t: &str) -> String {
        self.paint("1", t)
    }
    pub fn red(&self, t: &str) -> String {
        self.paint("31", t)
    }
    pub fn green(&self, t: &str) -> String {
        self.paint("32", t)
    }
    pub fn yellow(&self, t: &str) -> String {
        self.paint("33", t)
    }
    pub fn blue(&self, t: &str) -> String {
        self.paint("34", t)
    }
    pub fn magenta(&self, t: &str) -> String {
        self.paint("35", t)
    }
}

/// Render one event. Returns the text to print, or `None` to print nothing.
pub fn render(style: &Style, event: &Event) -> Option<String> {
    let line = match event {
        Event::SessionStarted {
            working_dir,
            model,
        } => Some(format!(
            "{} {}",
            style.dim("session"),
            style.dim(&format!("{model} in {working_dir}"))
        )),

        Event::UserMessage { .. } => return None,

        Event::ModelStarted => Some(style.dim("thinking...")),

        // Program source streams like anything else the model says.
        Event::ModelDelta { text } => Some(text.clone()),

        Event::ProgramStarted { round, .. } => {
            Some(format!("{} {}", style.dim("program"), style.dim(&format!("#{round}"))))
        }
        Event::ProgramFinished {
            duration_ms,
            ..
        } => Some(format!(
            "{} {}",
            style.dim("program finished in"),
            style.dim(&format!("{duration_ms}ms"))
        )),

        Event::CapabilityStarted { capability, args, .. } => {
            let detail = if args.is_empty() {
                String::new()
            } else {
                format!(" {args}")
            };
            // A neutral marker: whether the call was permitted is reported by
            // the finish event, and a tick here would read as "it worked" even
            // when the very next line says it was refused.
            Some(format!(
                "  {} {}{}",
                style.dim("\u{00b7}"),
                style.dim(capability),
                style.dim(&detail)
            ))
        }
        Event::CapabilityFinished {
            capability,
            ok,
            summary,
            ..
        } => {
            let mark = if *ok {
                style.green("\u{2713}")
            } else {
                style.red("\u{2717}")
            };
            Some(format!("  {mark} {} {}", style.dim(capability), style.dim(summary)))
        }
        Event::CapabilityOutput { chunk, .. } => {
            Some(format!("    {}", style.dim(chunk.trim_end())))
        }

        Event::FileChanged { path, change } => {
            let verb = match change {
                FileChange::Created => "created",
                FileChange::Modified => "modified",
                FileChange::Deleted => "deleted",
            };
            Some(format!("  {} {} {path}", style.yellow("\u{270e}"), style.dim(verb)))
        }

        Event::ProcessStarted { target, args, .. } => {
            Some(format!(
                "  {} {}",
                style.yellow("$"),
                style.dim(format!("{target} {}", args.join(" ")).trim_end())
            ))
        }
        Event::ProcessOutput { stream, chunk, .. } => {
            let prefix = match stream {
                OutputStream::Stdout => "    ",
                OutputStream::Stderr => "    ! ",
            };
            Some(format!("{prefix}{}", style.dim(chunk.trim_end())))
        }
        Event::ProcessFinished {
            exit_code,
            duration_ms,
            truncated,
            ..
        } => {
            let outcome = match exit_code {
                Some(0) => style.green("ok").to_string(),
                Some(code) => style.red(&format!("exit {code}")).to_string(),
                None => style.red("killed").to_string(),
            };
            let cut = if *truncated { style.dim(" (output truncated)") } else { String::new() };
            Some(format!(
                "    {} {}{cut}",
                outcome,
                style.dim(&format!("{duration_ms}ms"))
            ))
        }

        Event::MemoryWrite { key, kind } => Some(format!(
            "  {} {} {}",
            style.magenta("\u{1f4be}"),
            style.dim(&format!("{kind} saved")),
            style.dim(key)
        )),

        Event::UiPrompt { message, .. } => Some(format!(
            "  {} {}",
            style.blue("?"),
            style.bold(message)
        )),

        Event::Answer { text } => Some(format!("\n{}", style.bold(text))),

        Event::ProgramFailed {
            error, diagnostics, ..
        } => {
            let mut out = format!("  {} {}", style.red("program failed"), style.dim(&error.message));
            if let Some(diagnostics) = diagnostics {
                for line in diagnostics.lines().take(12) {
                    out.push('\n');
                    out.push_str(&format!("    {}", style.dim(line)));
                }
            }
            Some(out)
        }

        Event::Error { error } => Some(format!(
            "  {} {}",
            style.red("\u{2717}"),
            style.red(&error.to_string())
        )),

        Event::SessionFinished { status } => {
            let label = match status {
                SessionStatus::Completed => style.green("done"),
                SessionStatus::Failed => style.red("failed"),
                SessionStatus::Cancelled => style.yellow("cancelled"),
                _ => style.dim("idle"),
            };
            Some(format!("{} {label}", style.dim("session")))
        }
    };
    line.map(|l| format!("{l}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_protocol::{CallId, ErrorKind, ErrorPayload};

    fn style() -> Style {
        Style::plain()
    }

    fn line(event: &Event) -> String {
        render(&style(), event).unwrap_or_default()
    }

    #[test]
    fn the_user_message_is_not_echoed_back() {
        // The terminal already shows what was typed.
        assert!(line(&Event::UserMessage { text: "hello".into() }).is_empty());
    }

    #[test]
    fn a_capability_call_is_shown_with_its_arguments() {
        let text = line(&Event::CapabilityStarted {
            call_id: CallId(1),
            capability: "repo.find".into(),
            args: "\"authenticate\"".into(),
        });
        assert!(text.contains("repo.find"), "got {text}");
        assert!(text.contains("\"authenticate\""), "got {text}");
    }

    #[test]
    fn a_failed_capability_is_marked_as_a_failure() {
        let ok = line(&Event::CapabilityFinished {
            call_id: CallId(1),
            capability: "repo.find".into(),
            ok: true,
            summary: "3 matches".into(),
        });
        let failed = line(&Event::CapabilityFinished {
            call_id: CallId(1),
            capability: "repo.write".into(),
            ok: false,
            summary: "PermissionDenied".into(),
        });
        assert!(ok.contains("\u{2713}"), "got {ok}");
        assert!(failed.contains("\u{2717}"), "got {failed}");
        assert!(failed.contains("PermissionDenied"), "got {failed}");
    }

    #[test]
    fn a_file_change_names_the_path_and_the_verb() {
        let text = line(&Event::FileChanged {
            path: "src/auth.rs".into(),
            change: FileChange::Modified,
        });
        assert!(text.contains("modified"), "got {text}");
        assert!(text.contains("src/auth.rs"), "got {text}");
    }

    #[test]
    fn a_process_shows_its_command_and_exit_code() {
        let started = line(&Event::ProcessStarted {
            call_id: CallId(1),
            target: "cargo".into(),
            args: vec!["test".into()],
        });
        assert!(started.contains("cargo test"), "got {started}");

        let finished = line(&Event::ProcessFinished {
            call_id: CallId(1),
            exit_code: Some(101),
            duration_ms: 4200,
            truncated: false,
        });
        assert!(finished.contains("exit 101"), "got {finished}");
    }

    #[test]
    fn truncation_is_stated_rather_than_hidden() {
        let text = line(&Event::ProcessFinished {
            call_id: CallId(1),
            exit_code: Some(0),
            duration_ms: 10,
            truncated: true,
        });
        assert!(text.contains("truncated"), "got {text}");
    }

    #[test]
    fn the_answer_is_set_apart_from_the_work() {
        let text = line(&Event::Answer {
            text: "Authentication lives in src/auth.rs.".into(),
        });
        assert!(text.starts_with('\n'), "the answer should stand apart: {text}");
        assert!(text.contains("Authentication lives"));
    }

    #[test]
    fn a_failed_program_shows_its_diagnostics() {
        let text = line(&Event::ProgramFailed {
            call_id: CallId(1),
            error: ErrorPayload::new(ErrorKind::Script, "did not compile"),
            diagnostics: Some("error: expected `}`\n1 | let x = ;".into()),
        });
        assert!(text.contains("did not compile"), "got {text}");
        assert!(text.contains("expected `}`"), "got {text}");
    }

    #[test]
    fn stderr_is_distinguished_from_stdout() {
        let out = line(&Event::ProcessOutput {
            call_id: CallId(1),
            stream: OutputStream::Stdout,
            chunk: "running 1 test\n".into(),
        });
        let err = line(&Event::ProcessOutput {
            call_id: CallId(1),
            stream: OutputStream::Stderr,
            chunk: "warning: unused\n".into(),
        });
        assert!(!out.contains('!'), "stdout should not be marked: {out}");
        assert!(err.contains('!'), "stderr should be marked: {err}");
    }

    #[test]
    fn every_event_renders_without_panicking() {
        // A new event variant must not be able to crash the frontend.
        let events = vec![
            Event::SessionStarted {
                working_dir: "/work".into(),
                model: "m".into(),
            },
            Event::SessionFinished {
                status: SessionStatus::Completed,
            },
            Event::UserMessage { text: "x".into() },
            Event::ModelStarted,
            Event::ModelDelta { text: "x".into() },
            Event::ProgramStarted {
                call_id: CallId(1),
                round: 0,
                source: "x".into(),
            },
            Event::ProgramFinished {
                call_id: CallId(1),
                duration_ms: 1,
            },
            Event::CapabilityStarted {
                call_id: CallId(1),
                capability: "c".into(),
                args: String::new(),
            },
            Event::CapabilityOutput {
                call_id: CallId(1),
                chunk: "x".into(),
            },
            Event::CapabilityFinished {
                call_id: CallId(1),
                capability: "c".into(),
                ok: true,
                summary: String::new(),
            },
            Event::FileChanged {
                path: "p".into(),
                change: FileChange::Deleted,
            },
            Event::ProcessStarted {
                call_id: CallId(1),
                target: "t".into(),
                args: Vec::new(),
            },
            Event::ProcessOutput {
                call_id: CallId(1),
                stream: OutputStream::Stdout,
                chunk: "x".into(),
            },
            Event::ProcessFinished {
                call_id: CallId(1),
                exit_code: None,
                duration_ms: 1,
                truncated: false,
            },
            Event::MemoryWrite {
                key: "k".into(),
                kind: "program".into(),
            },
            Event::UiPrompt {
                call_id: CallId(1),
                message: "?".into(),
            },
            Event::Answer { text: "a".into() },
            Event::ProgramFailed {
                call_id: CallId(1),
                error: ErrorPayload::new(ErrorKind::Script, "x"),
                diagnostics: None,
            },
            Event::Error {
                error: ErrorPayload::new(ErrorKind::Model, "x"),
            },
        ];
        for event in events {
            let _ = render(&Style::plain(), &event);
            let _ = render(&Style::detect(), &event);
        }
    }

    #[test]
    fn colour_is_emitted_only_when_enabled() {
        let mut off = Style::plain();
        off.enabled = false;
        assert_eq!(off.red("x"), "x");

        let on = Style { enabled: true };
        assert!(on.red("x").contains("\u{1b}[31m"));
    }
}

