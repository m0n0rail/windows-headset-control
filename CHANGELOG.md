# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Razer BlackShark V2 Pro (`0x1532` / `0x0555`) support.** Its protocol was established
  from 26 capture sessions of Synapse driving the contributor's own headset, recorded in
  `docs/device-research-blackshark-v2-pro.md`, and nothing was carried over from the V3 Pro
  without a session of its own. It differs from the V3 Pro in transport (interrupt OUT
  rather than `SET_REPORT`), framing (several messages per report, no checksum), and
  features. It works without Synapse: a freshly plugged-in dongle answers only the link
  read until it has seen Synapse's `e1` request, so a session opens with that request, as
  captured. Verified on hardware with Synapse closed and the dongle freshly plugged in:
  reads of link, battery, power-saving, do not disturb and EQ preset and bank; do not
  disturb written and read back both ways; sidetone set from the tray and heard.
- `headsetctl get/set` gain `power-saving`, `do-not-disturb`, `eq-preset`, `eq-bank` and
  `mic-eq`, for the device that has them.

### Changed

- **Device access goes through per-product profiles and codecs.** A product is supported
  when it has a profile; its codec owns its identifiers, allowlists, ranges, framing, write
  preamble and refusal convention. The V3 Pro's bytes on the wire are unchanged.
- `set` and `param get/set` identify the device before validating, because the limits are
  per device. Without a headset attached they now report the missing device instead of an
  allowlist or range error.
- The tray hides controls the connected headset does not have, marks a sidetone level it
  cannot read back as `SENT`, and names the detected model in its tooltip.

### Fixed

- **The Windows microphone mute is read at startup again.** It was read before the UI thread
  had initialised COM, so the read failed every time and the mute pill stayed unknown until
  a headset event filled it in. On the V3 Pro the startup read of the hardware switch hid
  this; on the V2 Pro, which reports its switch only when it changes, the pill showed `--`.
- **The tray tooltip is shown.** The icon is registered under `NOTIFYICON_VERSION_4`, where
  the shell shows the standard tooltip only when `NIF_SHOWTIP` is set. It never was, so the
  tooltip text was kept up to date and never displayed.

## [0.2.0-alpha.4] - 2026-09-18

### Fixed

- **A dongle that has stopped answering now says so, instead of reading SEARCHING forever.**
  The dongle can wedge: it stays enumerated, Windows reports every interface healthy, the
  control collection still resolves and scores correctly, and it answers nothing at all.
  The tray had no way to describe that. `connected` was `Option<bool>` and `None` meant
  only "not read yet", which the panel drew as SEARCHING — the same thing it shows in the
  first second after launch — so a wedged dongle and a cold start looked identical, and
  neither the header nor the tooltip ever mentioned the one thing that clears it. This is
  not a guess about the cause: the first parameter the refresh asks for is answered by the
  dongle out of its own state rather than proxied over the wireless link, and it answers
  even with the headset powered off, so silence there is not "the headset is away" — it is
  the dongle itself having gone quiet. The header now reads NOT RESPONDING · REPLUG DONGLE
  and the tooltip says to unplug it and plug it back in.
- **Silence is no longer reported as a malformed response.** A request that went unanswered
  came back as `ProtocolMismatch`, the error that means the device replied and the reply
  was wrong. Two different conditions with different causes and different remedies shared
  one variant, and the only way to tell them apart was to match on the text of the message,
  so the tray did not try. Not answering at all is now its own error carrying the parameter,
  the wait, and how many unrelated events arrived. The wording callers see is unchanged.
- **The device thread stopped hammering a dongle that was never going to answer.** A failed
  refresh left the refresh timer untouched, so the worker re-ran the whole read sequence
  every couple of seconds — each attempt blocking for a full exchange timeout — for as long
  as the condition lasted, reporting it only at a log level nothing was listening to.
  Retries now back off, and once the dongle is judged unresponsive they drop to one attempt
  every fifteen seconds.

## [0.2.0-alpha.3] - 2026-09-04

### Fixed

- **Switch output when off stopped working, permanently and silently.** The record of where
  the sound came from doubles as the "we owe you a move back" flag, and its presence was
  read as "already switched". Endpoint ids do not live forever — a reinstalled driver or a
  re-enumerated dongle retires one — so once that record named an endpoint the machine no
  longer had, it could never be discharged and never be replaced. Every later power-off and
  power-on did nothing at all. A record now counts only while its endpoint still exists,
  and one that never reappears is given up on after about ten seconds rather than being
  kept forever.

### Added

- **The output switch says when it can't do its job.** No device chosen, the chosen device
  unplugged, Windows refusing the change, or the device you were on having disappeared: each
  raises a notification the first time and stays on the settings row until it clears.
  Previously all four were `tracing` calls at a level no subscriber was listening to.
- `headset-tray.exe --explain-output`, which reports what the switch would do for a
  powered-off and a powered-on headset, and why, without moving anything.
- **Split game and chat**, off by default. The headset presents two playback endpoints and
  Windows keeps a separate default for calls, so the two exist precisely to be pointed at
  different roles. Putting the sound *back* the way it was found cannot do that — it names
  one endpoint — and setting that one endpoint into all three of Windows' roles overwrote a
  communications default aimed at the chat channel, so the voices came out of the game
  channel. Turn the setting on, pick a **Game channel** and a **Chat channel**, and a
  headset coming back gets ordinary sound on the first and calls on the second. Left off,
  nothing about the switch changes.

## [0.1.0-alpha.2] - 2026-08-05

### Fixed

- **The game/chat slider ran backwards.** The panel labelled low values CHAT and high
  values GAME, so dragging toward GAME moved the mix toward chat. `0x00` is full game and
  `0x14` full chat — a direction the captures never established, since they record the
  range and the clamps but not which end is which.

### Added

- **Switch output when off.** Turning the headset off moves Windows' sound to a
  device you choose, and turning it back on puts it where it was. Off by default. The
  trigger is the headset's link state — no power-button event exists in the protocol — so
  an auto-sleep or going out of range counts too, debounced by two seconds. Setting the
  default output has no documented Windows API; see
  [`docs/undocumented-apis.md`](docs/undocumented-apis.md).
- A light theme. It follows your Windows setting by default, with an **Appearance** override
  in settings. High contrast still wins over both.

## [0.1.0-alpha.1] - 2026-08-02

First released build. Alpha: it speaks a protocol reconstructed by observation and has been
run against exactly one headset.

### Added

- **An installer.** A per-user setup executable with a Start menu entry, published on the
  releases page. No administrator rights, no driver, no service.
- **An application icon.** The headset the tray draws, embedded in the executable at five
  sizes, so Explorer, the Start menu, and the shortcut all show it.

- Noise control: off, ANC, or ambient, with an ANC level of 1–4. Exposed as `headsetctl
  noise` and as a segmented control in the tray panel. Parameter `0x12`, identified by
  capture — see `docs/device-research.md`.
- The tray icon can be operated from the keyboard: focus it and press Enter or Space.
- A high-contrast palette, applied when Windows high contrast is on.
- A stable notification-icon identity, so a pin-to-taskbar choice survives reinstalling.
- A Direct2D tray panel, replacing the context-menu-only interface.

### Fixed

- The tray icon no longer disappears permanently when Explorer restarts.
- The panel opens on the monitor its icon is on, rather than being clamped to the primary
  display's work area.
- The panel renders at the display's real pixel density instead of being stretched.
- The panel is placed clear of a taskbar docked to the left or right, instead of over it.
- A second launch raises the running instance instead of adding a duplicate tray icon.
- The panel holds its bottom edge when its height changes, instead of growing downward
  over the taskbar.
- Changing a noise setting updates the panel immediately rather than a beat later.

### Changed

- Dual-licensed under MIT OR Apache-2.0. Previously unlicensed and all rights reserved.
