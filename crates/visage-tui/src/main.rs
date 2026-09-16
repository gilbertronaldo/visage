//! `visage-enroll` — enrollment with a picture.
//!
//! Enrollment used to be blind: look at a lens, four captures happen, and
//! nothing tells you whether you were too dark, off-centre or out of frame.
//! This shows the camera's view while the daemon streams it, labels the frames
//! it would reject, and says which capture failed and why.
//!
//! ⚠️ `Enroll` is root-only, so this runs under `sudo`. That is fine for a
//! terminal and is precisely the thing a graphical front-end cannot do
//! gracefully — see the GUI prototype.

mod preview;

use anyhow::{Context, Result};
use clap::Parser;
use preview::{assess, fit, sample, Legibility, Preview};
use ratatui::crossterm::event::{self, Event, KeyCode};
use ratatui::crossterm::{execute, terminal};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use std::io::stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use visage_ipc::VisageProxy;

#[derive(Parser)]
#[command(name = "visage-enroll", about = "Enroll a face, with a live preview")]
struct Cli {
    /// User to enroll for. Defaults to the invoking user (SUDO_USER under sudo).
    #[arg(short, long)]
    user: Option<String>,

    /// Comma-separated capture labels, one per angle.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "normal,left,right,glasses"
    )]
    labels: Vec<String>,
}

/// Where the enrollment has got to. The render loop reads this; the enroll task
/// writes it.
#[derive(Debug, Clone)]
enum Phase {
    Connecting,
    /// Capturing `label`, the `n`th of `total`.
    Capturing {
        label: String,
        n: usize,
        total: usize,
    },
    Done,
}

#[derive(Debug, Clone)]
struct Outcome {
    label: String,
    ok: bool,
    detail: String,
}

struct App {
    phase: Phase,
    frame: Option<Preview>,
    outcomes: Vec<Outcome>,
    /// Set when the enroll task finishes, successfully or not.
    finished: Option<String>,
}

/// The invoking user, preferring `SUDO_USER`.
///
/// Mirrors the CLI's rule, and for the same reason: under the `sudo` that
/// `Enroll` requires, `$USER` is **root**, so defaulting to it enrolls the
/// operator's face against the root account and reports success.
fn current_user() -> Option<String> {
    std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && u != "root")
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| !u.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let user = match cli.user.or_else(current_user) {
        Some(u) if u != "root" => u,
        Some(_) => anyhow::bail!(
            "refusing to enrol root. Run this with sudo as your own user, or pass --user."
        ),
        None => anyhow::bail!("cannot determine which user to enrol; pass --user"),
    };
    if cli.labels.is_empty() {
        anyhow::bail!("no labels given — nothing to enrol");
    }

    let app = Arc::new(Mutex::new(App {
        phase: Phase::Connecting,
        frame: None,
        outcomes: Vec::new(),
        finished: None,
    }));

    let worker = tokio::spawn(enroll_all(app.clone(), user, cli.labels.clone()));
    let render = run_ui(app.clone());

    // The UI owns the terminal, so it must be torn down before anything else
    // prints — including a panic message from the worker.
    let ui_result = render;
    worker.abort();

    // Say it again on stdout, now that the alternate screen is gone.
    //
    // ratatui restores the previous screen contents on exit, so everything the
    // TUI drew — including the reason an enrollment failed — is destroyed the
    // moment the user quits. Measured on real hardware 2026-09-16: a run that
    // never reached Enroll spent 26 seconds displaying its diagnosis and left
    // NOTHING in the scrollback, so the failure could not be reported or acted
    // on. For a tool whose whole purpose is saying why enrollment failed,
    // losing the reason on exit defeats the point of building it.
    //
    // A poisoned lock is exactly when the report matters most, so recover the
    // inner value rather than panicking over it.
    let snapshot = match app.lock() {
        Ok(a) => report_lines(&a),
        Err(poisoned) => report_lines(&poisoned.into_inner()),
    };
    for line in snapshot {
        println!("{line}");
    }

    ui_result
}

/// The closing verdict, as lines.
///
/// Returned rather than printed so it can be asserted in a test: "the reason
/// reaches the user" is the property that matters, and a function that only
/// prints cannot be checked.
fn report_lines(a: &App) -> Vec<String> {
    let mut out = Vec::new();
    for o in &a.outcomes {
        out.push(format!(
            "{}  {}: {}",
            if o.ok { "ok    " } else { "FAILED" },
            o.label,
            o.detail
        ));
    }
    if let Some(msg) = &a.finished {
        out.push(msg.clone());
    }
    out
}

/// Drive the enrollment, one label at a time, recording what happened.
async fn enroll_all(app: Arc<Mutex<App>>, user: String, labels: Vec<String>) {
    let conn = match connect().await {
        Ok(c) => c,
        Err(e) => {
            let mut a = app.lock().unwrap();
            a.finished = Some(format!("could not reach visaged: {e}"));
            a.phase = Phase::Done;
            return;
        }
    };
    let proxy = match VisageProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            let mut a = app.lock().unwrap();
            a.finished = Some(format!("could not open the Visage interface: {e}"));
            a.phase = Phase::Done;
            return;
        }
    };

    // Subscribe BEFORE the first enroll: a stream opened afterwards would miss
    // the framing frames, which are the whole point.
    let mut frames = match proxy.receive_preview_frame().await {
        Ok(s) => s,
        Err(e) => {
            let mut a = app.lock().unwrap();
            a.finished = Some(format!("could not subscribe to preview frames: {e}"));
            a.phase = Phase::Done;
            return;
        }
    };

    let pump = {
        let app = app.clone();
        tokio::spawn(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            while let Some(sig) = frames.next().await {
                let Ok(args) = sig.args() else { continue };
                let mut a = app.lock().unwrap();
                a.frame = Some(Preview {
                    width: args.width,
                    height: args.height,
                    data: args.data.clone(),
                    is_dark: args.is_dark,
                });
            }
        })
    };

    let total = labels.len();
    for (i, label) in labels.iter().enumerate() {
        {
            let mut a = app.lock().unwrap();
            a.phase = Phase::Capturing {
                label: label.clone(),
                n: i + 1,
                total,
            };
        }
        let outcome = match proxy.enroll(&user, label).await {
            Ok(id) => Outcome {
                label: label.clone(),
                ok: true,
                detail: id,
            },
            Err(e) => Outcome {
                label: label.clone(),
                ok: false,
                detail: e.to_string(),
            },
        };
        app.lock().unwrap().outcomes.push(outcome);
    }

    pump.abort();

    let mut a = app.lock().unwrap();
    let good = a.outcomes.iter().filter(|o| o.ok).count();
    a.finished = Some(if good == 0 {
        // Never let a failed enrollment look like a successful one.
        "nothing was enrolled.".to_string()
    } else {
        format!("enrolled {good} of {total} angles for this user.")
    });
    a.phase = Phase::Done;
}

async fn connect() -> Result<zbus::Connection> {
    let builder = if visage_ipc::session_bus_from_env() {
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };
    // Framing adds time to every Enroll, and the default 10s budget is what
    // stands between a slow enrollment and a failure indistinguishable from a
    // bad capture.
    builder
        .method_timeout(Duration::from_secs(60))
        .build()
        .await
        .context("connecting to D-Bus")
}

fn run_ui(app: Arc<Mutex<App>>) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = ui_loop(&mut term, app);

    // Restore unconditionally. A UI that leaves the terminal in raw mode on the
    // way out is worse than one that never ran.
    let _ = terminal::disable_raw_mode();
    let _ = execute!(stdout(), terminal::LeaveAlternateScreen);
    let _ = term.show_cursor();
    result
}

fn ui_loop(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: Arc<Mutex<App>>,
) -> Result<()> {
    loop {
        let snapshot = {
            let a = app.lock().unwrap();
            (
                a.phase.clone(),
                a.frame.clone(),
                a.outcomes.clone(),
                a.finished.clone(),
            )
        };
        term.draw(|f| draw(f, &snapshot))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    return Ok(());
                }
                if snapshot.3.is_some() {
                    return Ok(());
                }
            }
        }
    }
}

type Snapshot = (Phase, Option<Preview>, Vec<Outcome>, Option<String>);

fn draw(f: &mut Frame, (phase, frame, outcomes, finished): &Snapshot) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(outcomes.len().clamp(1, 6) as u16 + 2),
    ])
    .split(f.area());

    let heading = match phase {
        Phase::Connecting => "connecting to visaged…".to_string(),
        Phase::Capturing { label, n, total } => format!("[{n}/{total}]  look {label}"),
        Phase::Done => finished.clone().unwrap_or_else(|| "done".into()),
    };
    f.render_widget(
        Paragraph::new(heading).block(Block::default().borders(Borders::ALL).title(" visage ")),
        rows[0],
    );

    let inner = rows[1].inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let verdict = frame.as_ref().map_or(Legibility::NoSignal, assess);
    let body = match frame {
        Some(p) if verdict != Legibility::NoSignal => halfblocks(p, inner.width, inner.height),
        _ => vec![Line::from(Legibility::NoSignal.advice())],
    };
    f.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", verdict.advice())),
        ),
        rows[1],
    );

    let lines: Vec<Line> = outcomes
        .iter()
        .map(|o| {
            let (mark, colour) = if o.ok {
                ("ok  ", Color::Green)
            } else {
                ("fail", Color::Red)
            };
            Line::from(vec![
                Span::styled(mark, Style::default().fg(colour)),
                Span::raw(format!("  {}  {}", o.label, o.detail)),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(if lines.is_empty() {
            vec![Line::from("no captures yet")]
        } else {
            lines
        })
        .block(Block::default().borders(Borders::ALL).title(" captures ")),
        rows[2],
    );
}

/// Render a frame as half-blocks: one cell carries two vertical pixels, the
/// upper as the glyph's foreground and the lower as its background.
///
/// This needs no terminal graphics protocol — only 24-bit colour, which is
/// effectively universal. Whether this terminal also speaks the Kitty graphics
/// protocol is unverified, so nothing here depends on it.
fn halfblocks(p: &Preview, cols: u16, rows: u16) -> Vec<Line<'static>> {
    let (w, h) = fit(p, cols, rows);
    if w == 0 || h == 0 {
        return vec![Line::from("")];
    }
    let px = sample(p, w, h);
    (0..h / 2)
        .map(|row| {
            let spans = (0..w)
                .map(|x| {
                    let top = px[row * 2 * w + x];
                    let bottom = px[(row * 2 + 1) * w + x];
                    Span::styled(
                        "▀",
                        Style::default()
                            .fg(Color::Rgb(top, top, top))
                            .bg(Color::Rgb(bottom, bottom, bottom)),
                    )
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason an enrollment failed must outlive the TUI.
    ///
    /// ratatui restores the previous screen on exit, so anything only drawn
    /// inside the alternate screen is gone the moment the user quits. On real
    /// hardware that turned a 26-second on-screen diagnosis into an empty
    /// scrollback and an unreportable failure.
    #[test]
    fn the_failure_reason_reaches_stdout_after_the_alternate_screen_is_gone() {
        let app = App {
            phase: Phase::Done,
            frame: None,
            outcomes: vec![
                Outcome {
                    label: "normal".to_string(),
                    ok: false,
                    detail: "no usable frames captured".to_string(),
                },
                Outcome {
                    label: "left".to_string(),
                    ok: true,
                    detail: "a-model-uuid".to_string(),
                },
            ],
            finished: Some("enrolled 1 of 2 angles for this user.".to_string()),
        };

        let lines = report_lines(&app);

        assert!(
            lines
                .iter()
                .any(|l| l.contains("normal") && l.contains("no usable frames")),
            "the specific reason a capture failed must survive; got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("FAILED")),
            "a failure must be visibly marked, not just listed; got {lines:?}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some("enrolled 1 of 2 angles for this user."),
            "the closing summary must come last; got {lines:?}"
        );
    }

    /// Negative control: with nothing recorded there is nothing to print, so a
    /// pass above cannot come from a function that always emits something.
    #[test]
    fn an_empty_run_reports_nothing_rather_than_a_reassuring_blank() {
        let app = App {
            phase: Phase::Connecting,
            frame: None,
            outcomes: Vec::new(),
            finished: None,
        };
        assert!(report_lines(&app).is_empty());
    }

    /// Both cases in one test on purpose.
    ///
    /// `set_var` mutates process-global state and cargo runs a binary's tests
    /// in parallel threads, so two tests each setting `USER` would race and
    /// fail intermittently — the worst kind of failure, because it looks like
    /// flakiness rather than a defect.
    #[test]
    fn user_resolution_prefers_sudo_user_and_can_still_yield_root() {
        std::env::set_var("SUDO_USER", "alice");
        std::env::set_var("USER", "root");
        assert_eq!(
            current_user(),
            Some("alice".to_string()),
            "SUDO_USER must win, or enrolling under sudo targets the wrong account"
        );

        // The trap the CLI shipped with: with no SUDO_USER, $USER under sudo is
        // root. This value is reachable, which is exactly why main() refuses it
        // rather than trusting it.
        std::env::remove_var("SUDO_USER");
        std::env::set_var("USER", "root");
        assert_eq!(current_user(), Some("root".to_string()));
    }
}
