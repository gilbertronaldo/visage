//! Turning a `PreviewFrame` into something a person can act on.
//!
//! Everything here is pure: a frame in, a verdict or a pixel grid out. The
//! rendering loop is untestable without a terminal and a camera; these are the
//! parts with a computable answer, so they are separated out and tested.

/// One frame as it arrives over the bus.
#[derive(Debug, Clone)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// 8-bit grayscale, row-major, `width * height` bytes.
    pub data: Vec<u8>,
    /// The daemon's verdict: this frame was too dark to use.
    pub is_dark: bool,
}

/// What to tell the user about this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legibility {
    /// Usable. Say nothing; the picture speaks.
    Ok,
    /// The daemon rejected it as too dark.
    TooDark,
    /// Saturated — almost certainly the IR emitter washing the sensor out
    /// before auto-gain has settled.
    TooBright,
    /// Nothing arrived, or the frame is malformed.
    NoSignal,
}

impl Legibility {
    /// What the user should be told. Short, actionable, no jargon.
    pub fn advice(self) -> &'static str {
        match self {
            Legibility::Ok => "looking good — hold still",
            Legibility::TooDark => "too dark — more light, or move closer",
            Legibility::TooBright => "washed out — move back from the camera",
            Legibility::NoSignal => "waiting for the camera…",
        }
    }
}

/// Mean brightness at or above which a frame is treated as washed out.
///
/// The daemon cannot tell you this. `is_dark_frame` counts pixels *below* 32,
/// so it detects darkness and nothing else — the white-out that a mis-warmed IR
/// emitter produces arrives flagged as a perfectly good frame. The client holds
/// the pixels, so the client is where saturation can be noticed at all.
const SATURATED_MEAN: u8 = 200;

/// Classify a frame for display.
pub fn assess(frame: &Preview) -> Legibility {
    if frame.data.is_empty() || frame.width == 0 || frame.height == 0 {
        return Legibility::NoSignal;
    }
    // The daemon's verdict wins where it has one: it ran the same threshold the
    // capture loop uses, so agreeing with it keeps the preview honest about why
    // a capture will be rejected.
    if frame.is_dark {
        return Legibility::TooDark;
    }
    let sum: u64 = frame.data.iter().map(|&p| u64::from(p)).sum();
    let mean = (sum / frame.data.len() as u64) as u8;
    if mean >= SATURATED_MEAN {
        Legibility::TooBright
    } else {
        Legibility::Ok
    }
}

/// Nearest-neighbour resample to an exact `out_w * out_h` grid.
///
/// Nearest-neighbour on purpose: this runs every frame on a UI thread, and a
/// preview does not need to be pretty. Returns exactly `out_w * out_h` bytes so
/// a renderer can index it without bounds-checking every pixel.
pub fn sample(frame: &Preview, out_w: usize, out_h: usize) -> Vec<u8> {
    if out_w == 0 || out_h == 0 || frame.data.is_empty() {
        return Vec::new();
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let mut out = Vec::with_capacity(out_w * out_h);
    for y in 0..out_h {
        let sy = (y * h / out_h).min(h.saturating_sub(1));
        for x in 0..out_w {
            let sx = (x * w / out_w).min(w.saturating_sub(1));
            // Clamp rather than trust: a frame whose `data` is shorter than
            // `width * height` is malformed, and rendering garbage is worse
            // than rendering black.
            out.push(frame.data.get(sy * w + sx).copied().unwrap_or(0));
        }
    }
    out
}

/// Fit `out_w x out_h` inside `cols x rows` cells while preserving aspect.
///
/// Each cell carries two vertical pixels (an upper half-block over a coloured
/// background), so the pixel grid is twice as tall as the cell grid.
pub fn fit(frame: &Preview, cols: u16, rows: u16) -> (usize, usize) {
    if cols == 0 || rows == 0 || frame.width == 0 || frame.height == 0 {
        return (0, 0);
    }
    let (fw, fh) = (frame.width as f32, frame.height as f32);
    let (max_w, max_h) = (cols as f32, rows as f32 * 2.0);
    let scale = (max_w / fw).min(max_h / fh);
    let w = (fw * scale).floor().max(1.0) as usize;
    // Height must be even so every cell has both of its pixels.
    let h = ((fh * scale).floor().max(2.0) as usize) & !1;
    (w, h.max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8, is_dark: bool) -> Preview {
        Preview {
            width: w,
            height: h,
            data: vec![fill; (w * h) as usize],
            is_dark,
        }
    }

    #[test]
    fn an_empty_frame_is_no_signal_not_darkness() {
        // These are different things to tell a user. "Too dark" sends them
        // looking for a lamp; "waiting" tells them nothing is arriving.
        let mut f = frame(4, 4, 0, false);
        f.data.clear();
        assert_eq!(assess(&f), Legibility::NoSignal);
    }

    #[test]
    fn the_daemons_dark_verdict_is_honoured() {
        assert_eq!(assess(&frame(4, 4, 10, true)), Legibility::TooDark);
    }

    /// The case the daemon structurally cannot report.
    #[test]
    fn saturation_is_detected_client_side() {
        let f = frame(4, 4, 250, false);
        assert!(!f.is_dark, "the daemon considers this frame fine");
        assert_eq!(assess(&f), Legibility::TooBright);
    }

    #[test]
    fn an_ordinary_frame_is_ok() {
        assert_eq!(assess(&frame(4, 4, 120, false)), Legibility::Ok);
    }

    #[test]
    fn sampling_returns_exactly_the_requested_grid() {
        for (ow, oh) in [(1, 2), (7, 4), (160, 90), (3, 3)] {
            let got = sample(&frame(160, 90, 42, false), ow, oh);
            assert_eq!(got.len(), ow * oh, "{ow}x{oh}");
        }
    }

    /// Without this, every test above passes on a sampler returning zeros.
    #[test]
    fn sampling_reads_the_source() {
        let mut f = frame(8, 8, 0, false);
        for (i, px) in f.data.iter_mut().enumerate() {
            *px = (i % 8) as u8 * 30;
        }
        let out = sample(&f, 8, 8);
        let distinct: std::collections::HashSet<u8> = out.iter().copied().collect();
        assert!(
            distinct.len() > 1,
            "sampler produced a flat image: {distinct:?}"
        );
    }

    /// A short `data` is malformed input, not a reason to panic in a UI loop.
    #[test]
    fn a_truncated_frame_does_not_panic() {
        let mut f = frame(160, 90, 7, false);
        f.data.truncate(10);
        let out = sample(&f, 40, 20);
        assert_eq!(out.len(), 40 * 20);
    }

    #[test]
    fn fit_preserves_aspect_and_stays_inside_the_cells() {
        let f = frame(160, 90, 0, false);
        let (w, h) = fit(&f, 80, 24);
        assert!(w <= 80, "width {w} overflows 80 cols");
        assert!(h <= 48, "height {h} overflows 24 rows of 2 pixels");
        assert_eq!(h % 2, 0, "height must be even so every cell has two pixels");
        let want = 160.0 / 90.0;
        let got = w as f32 / h as f32;
        assert!((got - want).abs() < 0.2, "aspect drifted: {got} vs {want}");
    }

    #[test]
    fn fit_on_a_zero_sized_terminal_is_empty_not_a_panic() {
        assert_eq!(fit(&frame(160, 90, 0, false), 0, 0), (0, 0));
    }
}
