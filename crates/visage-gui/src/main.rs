//! `visage-enroll-gui` — the same enrollment, in a window.
//!
//! This exists to be compared against `visage-enroll` (the TUI), not because a
//! GUI was assumed to be better. Same daemon, same signal, same state machine,
//! same per-capture verdicts — only the renderer differs, so a comparison
//! measures the interaction model rather than two different programs.
//!
//! ⛔ **READ THIS BEFORE JUDGING IT ON LOOKS.** `Enroll` is root-only: checked
//! in-process by `require_root_caller`, and again by omission from the D-Bus
//! policy's default context. So this window must run as root.
//!
//! Running a GUI under `sudo` is not a cosmetic wart. It needs the invoking
//! user's `WAYLAND_DISPLAY`/`DISPLAY` and `XDG_RUNTIME_DIR` forwarded, which
//! `sudo` strips by default; where it works at all it is because the display
//! server was talked into trusting a root client. No mainstream desktop user
//! should be asked to do this, and no distribution should ship a `.desktop`
//! entry that does.
//!
//! There is **no polkit integration in this codebase** — verified, not assumed:
//! zero source hits, and the only mentions anywhere are polkit as a *consumer*
//! of Visage's PAM module. So the honest options are a polkit-authorized path in
//! the daemon, a small setuid-free root helper the GUI talks to, or this stays a
//! developer prototype.
//!
//! That is the finding. A prototype that surfaces it is doing its job.

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use visage_ipc::VisageProxy;

#[derive(Parser)]
#[command(
    name = "visage-enroll-gui",
    about = "Enroll a face, in a window (prototype)"
)]
struct Cli {
    #[arg(short, long)]
    user: Option<String>,

    #[arg(
        long,
        value_delimiter = ',',
        default_value = "normal,left,right,glasses"
    )]
    labels: Vec<String>,
}

#[derive(Debug, Clone)]
struct Shot {
    label: String,
    ok: bool,
    detail: String,
}

#[derive(Default)]
struct Shared {
    heading: String,
    /// Latest frame: (width, height, grayscale bytes, daemon's dark verdict).
    frame: Option<(u32, u32, Vec<u8>, bool)>,
    shots: Vec<Shot>,
    finished: Option<String>,
}

/// Mean brightness at or above which a frame is washed out.
///
/// Same reasoning as the TUI's copy: the daemon's `is_dark_frame` counts pixels
/// *below* 32, so it cannot see saturation at all. Whoever holds the pixels is
/// the only one who can notice.
const SATURATED_MEAN: u8 = 200;

fn advice(frame: Option<&(u32, u32, Vec<u8>, bool)>) -> &'static str {
    match frame {
        None => "waiting for the camera…",
        Some((_, _, data, _)) if data.is_empty() => "waiting for the camera…",
        Some((_, _, _, true)) => "too dark — more light, or move closer",
        Some((_, _, data, _)) => {
            let mean = (data.iter().map(|&p| u64::from(p)).sum::<u64>() / data.len() as u64) as u8;
            if mean >= SATURATED_MEAN {
                "washed out — move back from the camera"
            } else {
                "looking good — hold still"
            }
        }
    }
}

fn current_user() -> Option<String> {
    std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && u != "root")
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| !u.is_empty())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let user = match cli.user.or_else(current_user) {
        Some(u) if u != "root" => u,
        Some(_) => {
            anyhow::bail!("refusing to enrol root. Run as your own user via sudo, or pass --user.")
        }
        None => anyhow::bail!("cannot determine which user to enrol; pass --user"),
    };

    // Fail with an explanation rather than a blank window. Under `sudo` the
    // display variables are usually stripped, and eframe's own error for that
    // is opaque.
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        anyhow::bail!(
            "no WAYLAND_DISPLAY or DISPLAY.\n\n\
             This is the GUI's core problem, not a misconfiguration on your part: \
             Enroll is root-only, sudo strips the display environment, and a graphical \
             client therefore cannot reach your session. Use `visage-enroll` (the TUI), \
             which has no such difficulty, or see this crate's module docs for the \
             polkit/helper options that would fix it properly."
        );
    }

    let shared = Arc::new(Mutex::new(Shared {
        heading: "connecting to visaged…".into(),
        ..Default::default()
    }));

    let worker = shared.clone();
    let labels = cli.labels.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(enroll_all(worker, user, labels));
    });

    let app = App { shared, tex: None };
    eframe::run_native(
        "Visage — enrol",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("could not open a window: {e}"))
}

async fn enroll_all(shared: Arc<Mutex<Shared>>, user: String, labels: Vec<String>) {
    let fail = |msg: String| {
        let mut s = shared.lock().unwrap();
        s.finished = Some(msg);
    };

    let builder = if visage_ipc::session_bus_from_env() {
        zbus::connection::Builder::session()
    } else {
        zbus::connection::Builder::system()
    };
    // Framing lengthens every Enroll; the default 10s budget is what separates
    // a slow enrollment from a failure that looks identical to a bad capture.
    let conn = match builder {
        Ok(b) => match b.method_timeout(Duration::from_secs(60)).build().await {
            Ok(c) => c,
            Err(e) => return fail(format!("could not reach visaged: {e}")),
        },
        Err(e) => return fail(format!("could not reach visaged: {e}")),
    };

    let proxy = match VisageProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => return fail(format!("could not open the Visage interface: {e}")),
    };

    // Subscribe before the first Enroll, or the framing frames are missed.
    let mut frames = match proxy.receive_preview_frame().await {
        Ok(s) => s,
        Err(e) => return fail(format!("could not subscribe to preview frames: {e}")),
    };

    let pump_shared = shared.clone();
    let pump = tokio::spawn(async move {
        use zbus::export::ordered_stream::OrderedStreamExt;
        while let Some(sig) = frames.next().await {
            let Ok(a) = sig.args() else { continue };
            let mut s = pump_shared.lock().unwrap();
            s.frame = Some((a.width, a.height, a.data.clone(), a.is_dark));
        }
    });

    let total = labels.len();
    for (i, label) in labels.iter().enumerate() {
        shared.lock().unwrap().heading = format!("[{}/{}]  look {}", i + 1, total, label);
        let shot = match proxy.enroll(&user, label).await {
            Ok(id) => Shot {
                label: label.clone(),
                ok: true,
                detail: id,
            },
            Err(e) => Shot {
                label: label.clone(),
                ok: false,
                detail: e.to_string(),
            },
        };
        shared.lock().unwrap().shots.push(shot);
    }
    pump.abort();

    let mut s = shared.lock().unwrap();
    let good = s.shots.iter().filter(|x| x.ok).count();
    s.finished = Some(if good == 0 {
        "nothing was enrolled.".into()
    } else {
        format!("enrolled {good} of {total} angles.")
    });
}

struct App {
    shared: Arc<Mutex<Shared>>,
    tex: Option<egui::TextureHandle>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        let (heading, frame, shots, finished) = {
            let s = self.shared.lock().unwrap();
            (
                s.heading.clone(),
                s.frame.clone(),
                s.shots.clone(),
                s.finished.clone(),
            )
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(finished.clone().unwrap_or(heading));
            ui.label(advice(frame.as_ref()));
            ui.separator();

            if let Some((w, h, data, _)) = &frame {
                if !data.is_empty() && *w > 0 && *h > 0 {
                    // Grayscale to RGB; egui has no single-channel image type.
                    let rgb: Vec<u8> = data.iter().flat_map(|&p| [p, p, p]).collect();
                    let img = egui::ColorImage::from_rgb([*w as usize, *h as usize], &rgb);
                    let tex = self.tex.get_or_insert_with(|| {
                        ctx.load_texture("preview", img.clone(), Default::default())
                    });
                    tex.set(img, Default::default());
                    ui.image((tex.id(), egui::vec2(*w as f32 * 2.0, *h as f32 * 2.0)));
                }
            } else {
                ui.label("no frames yet");
            }

            ui.separator();
            for s in &shots {
                let colour = if s.ok {
                    egui::Color32::GREEN
                } else {
                    egui::Color32::RED
                };
                ui.colored_label(colour, format!("{}  {}", s.label, s.detail));
            }
        });

        // The frame pump runs off-thread, so repaint on a timer rather than
        // waiting for input — otherwise the preview only updates when the mouse
        // moves, which looks exactly like a frozen camera.
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frame_and_an_empty_frame_both_read_as_waiting() {
        assert_eq!(advice(None), "waiting for the camera…");
        assert_eq!(
            advice(Some(&(160, 90, vec![], false))),
            "waiting for the camera…"
        );
    }

    #[test]
    fn the_daemons_dark_verdict_is_honoured() {
        assert!(advice(Some(&(4, 4, vec![10; 16], true))).contains("too dark"));
    }

    /// The saturation case the daemon cannot report, mirrored from the TUI so
    /// both front-ends say the same thing about the same frame.
    #[test]
    fn saturation_is_detected_client_side() {
        assert!(advice(Some(&(4, 4, vec![250; 16], false))).contains("washed out"));
    }

    #[test]
    fn an_ordinary_frame_is_ok() {
        assert!(advice(Some(&(4, 4, vec![120; 16], false))).contains("looking good"));
    }
}
