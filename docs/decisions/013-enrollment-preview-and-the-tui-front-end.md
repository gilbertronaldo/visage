# ADR 013 — Enrollment Preview, and Shipping a TUI Rather Than a GUI

**Date:** 2026-09-16
**Status:** Implemented
**Scope:** `visaged`, `visage-hw`, `visage-ipc`, `visage-tui`, packaging, NixOS module

---

## Context

Enrollment was blind. The user was told to look at a lens, captures happened, and nothing
said whether they were too dark, off-centre or out of frame. When a capture failed there
was no way to tell why, so the retry was the same guess again.

That is a first-run problem, and first-run success is what decides whether someone keeps
face authentication enabled or turns it off and forgets about it.

v0.4.0 shipped a `PreviewFrame` D-Bus signal that made frames visible to a client.
**Nothing consumed it.** This ADR covers what was built on top of it, and the decision of
which shape of front-end to ship.

## Decision

### 1. A framing phase before each capture

`run_enroll` streams frames to the enrolling client for `VISAGE_FRAMING_MS` (default
1500 ms, `0` disables) before capturing anything, so the user can position themselves
while seeing live feedback. The frames are discarded — they never reach the model.

**Bounded by time, not by frame count.** `capture_frames_observed` runs `max_attempts =
count * 3` and credits only *non-dark* frames toward its target. On hardware where most
frames read dark — an unquirked emitter, which is exactly where a preview helps most —
asking for N frames can block for `N * 3` dequeues. A framing phase is "stream for
~1.5s", not "collect N good frames", so `Camera::stream_frames_for` takes a deadline.

**A client that does not subscribe pays nothing.** `framing_for(has_preview, configured)`
returns zero when no preview channel exists, so scripted and headless enrollments keep
their old latency.

### 2. One shared D-Bus proxy (`visage-ipc`)

The `#[zbus::proxy]` trait lived inline in `visage-cli`. Three clients would have meant
three copies, and this repo has already shipped duplicate-definition drift twice (the
compatibility table vs. the shipped quirks, then the README's copy of that table). One
definition also gives `dbus_contract.rs` a single place to look.

### 3. Ship `visage-enroll` (TUI). Delete the GUI.

Two front-ends were built deliberately — `visage-tui` (ratatui) and `visage-gui` (egui) —
to settle by use, not by argument, which shape helps a first-time user. The terminal one
ships in the `.deb`, the RPM, the AUR package and Nix. The GUI is **deleted, not parked**.

## Rationale

**Why a preview at all.** The audience is mainstream laptop users and the metric is
first-run success. A failed enrollment with no explanation is the failure mode that makes
someone give up, and it is entirely avoidable: the daemon already has the pixels.

**Why the TUI won, and it is not a matter of taste.** `Enroll` is root-only — gated twice,
by `require_root_caller` and by omission from the D-Bus policy default context. A TUI
under `sudo` is ordinary. **A GUI under `sudo` is not**: it fights Wayland/X authorization
and `XDG_RUNTIME_DIR`, and it is a security smell to ask a mainstream user to accept.
Making the GUI viable needs polkit or a root helper, neither of which exists here. That is
real work to answer a question the comparison had already closed.

**Why half-block Unicode rather than a terminal graphics protocol.** `▀` with a foreground
and background colour encodes two vertical pixels, so truecolor alone renders the preview.
The plan flagged that Kitty graphics support should be verified empirically rather than
assumed; it never needed verifying, because the fallback turned out to be good enough that
the question is moot. One less external dependency to break.

**Why "too bright" is computed client-side.** The daemon's `is_dark_frame` counts pixels
below 32 — it detects darkness only, so the white-out that #104 fixed produces frames the
daemon considers good and never flags. The client has the pixels, so saturation labelling
is free there, and it keeps a presentation judgement where a UI can iterate on it.

## Trade-offs accepted

**Enrollment is slower.** Framing adds ~1.5s per label, and `onboard` captures four
labels, so a preview-using enrollment goes from roughly 1s to ~3s per `Enroll`. That sits
inside the connection-wide 10s method timeout, but that budget is now the thing standing
between a slow enrollment and a failure indistinguishable from a bad capture.

**The framing phase is not a no-op on capture conditions.** Sensor auto-gain adapts only
while the camera is streaming. Holding the stream open acts as an extended warmup and
leaves AGC in a different state than a bare capture would. The direction is probably
beneficial — it is the same mechanism behind the #104 fix — but it is a real behavioural
change, not a purely additive feature.

**The adjustment loop is retry, not a ready-gate.** There is no client→daemon channel
mid-call, and a standalone "stream me frames" method is exactly what the security policy
forbids. So the user gets a fixed framing window and then it captures. Windows Hello lets
you keep adjusting until it succeeds; within this constraint we cannot. A failed capture's
retry *is* the adjustment loop, which is why per-capture failure reasons matter so much.

**Deleting the GUI forecloses the graphical path.** If polkit integration ever lands, the
GUI would have to be rebuilt. Accepted: the code is in git history, and carrying an
unbuildable-in-practice prototype has an ongoing cost in dependency surface and reader
confusion.

## Expected benefits

- **A failed capture now says why.** Dark frames are rendered and *labelled* rather than
  silently dropped; saturation is flagged where the daemon cannot see it.
- **Measured on real hardware** (Shinetech `3277:0055`, 2026-09-16): 4 of 4 angles
  enrolled, best-face confidence 0.8657, preview legible at the 160px cap.
- **199 fewer packages** in the lockfile (572 → 373) from dropping the GUI — the entire
  `eframe`/`winit`/`glutin`/`wgpu` tree, nothing added, no version change to any surviving
  dependency. On a component sitting on the authentication path, that reduction in
  dependency surface is the more valuable half of the decision.
- **No new runtime dependency** in any packaging channel: ratatui and crossterm are pure
  Rust, so `depends` / `requires` lists are untouched.

## Drawbacks and known limitations

⛔ **The preview is a burst of stills, not a viewfinder, outside the framing window.**
`frames_per_enroll` defaults to 5 and `max_attempts = count * 3`, so one `Enroll` emits
5–15 frames over roughly half a second. The framing phase is what makes it feel continuous.

⛔ **Session-bus mode skips `require_root_caller`.** Testing on the session bus exercises
rendering but *not* the authorization that guards the preview in production. The hardware
validation above was deliberately run on the **system** bus with a root client so the gate
actually fired; any future session-bus testing carries this caveat.

⚠️ **The 160px cap is a policy, not a mechanism.** `PREVIEW_MAX_EDGE` is pinned by a test
that asserts the literal value, because a test comparing the constant against itself would
be a tautology. Nothing outside that test stops a future change from raising it.

⚠️ **`visage-enroll` runs under `sudo` and so does everything it links.** Shipping it
widens the code that executes as root during enrollment. It is a small crate over a shared
proxy, but it is not zero.

⚠️ **Unexplained: one run never reached `Enroll`.** On 2026-09-16 a run spent 26 seconds
in the TUI without invoking the method, then succeeded on the next attempt with no code
change. A hypothesis exists (connecting fractionally before the daemon was ready, since
the bus name appears the instant it is claimed) but it is **not established**. Recorded
rather than tidied away.

⚠️ **Eleven `Verify` failures on that host across three days remain unexplained.** They
are real and logged as "only dark or unreadable frames"; four consecutive successes the
same evening did not reproduce them. The preview does not explain them — it makes the next
one diagnosable.

## Remaining work

1. **Strobe-differential liveness.** The highest-value item on this hardware.
   `liveness.minDisplacement` is set to 0.1 on the-first — a gate that barely gates —
   because the landmark metric measurably cannot separate a phone-screen spoof from a live
   face on `3277:0055` (ADR 011's own hardware validation). Meanwhile this module's IR
   emitter strobes lit/unlit every frame by firmware default, confirmed at 10 good / 9 dark
   frames with mean brightness 54.8. A live face reflects the emitter and alternates
   strongly; a self-emissive display does not. `threat-model.md` already lists odd/even
   frame analysis as roadmap, and the signal is being produced with nothing consuming it.

2. **A contract test for the `preview_frame` signal.** `dbus_contract.rs` deliberately
   skips signal declarations, because the server's declaration carries a `SignalEmitter`
   that never reaches the wire and would fail a naive arity check. Filtering that one
   parameter would make the check real. The signatures currently agree — verified by
   reading them, which is exactly the kind of assurance this repo has learned not to trust.

3. **Deploy.** the-first still runs `0.4.0-rc.1`. The esver-os pin is bumped and staged;
   the node gets none of this until a rebuild.

4. **A quirk entry for `3277:0055`, or a documented decision not to.** `visage discover`
   reports "no quirk" and the emitter fires anyway. The absence is currently explained in
   three separate documents, which is a sign it belongs in the quirk database as an
   explicit "firmware-driven, no control needed" entry.

## References

- ADR 011 — passive liveness; its hardware validation is why item 1 above matters
- `docs/hardware-reports/asus-zenbook-um3406ha-3277-0055.md` — the strobe measurement
- `crates/visaged/src/engine.rs` — framing phase, `to_preview`, `PREVIEW_MAX_EDGE`
- `crates/visage-tui/src/preview.rs` — legibility assessment and downscaling
