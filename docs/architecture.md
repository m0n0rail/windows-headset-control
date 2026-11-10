# Architecture

## Crates

| Crate | Responsibility | May touch the OS |
| ----- | -------------- | ----------------- |
| `headset-protocol` | Pure protocol logic. `codec` holds the shared vocabulary (`Param`, `Role`, `ParamFrame`) and the `Codec` trait; each device's framing, identifiers and allowlists live in a module of their own (`v3pro`, with its byte-62 XOR checksum, established by observation — see docs/device-research.md). Encoding goes through a codec, so an identifier its device was not observed accepting cannot be encoded anywhere in the workspace. No dependency on `headset-device`. | No |
| `headset-device` | All Windows HID access behind a mockable `HidBackend` trait. Real windows-rs implementation (`WindowsHidBackend`) and a fixture-driven fake (`FakeHidBackend`) for tests. Also owns `ControlSession`: identity-based device resolution and request/response correlation. Depends on `headset-protocol`; the reverse dependency stays forbidden. | Yes |
| `headset-cli` | `headsetctl.exe`. Command dispatch, filtering, ranking, redaction, and rendering, all built above the `HidBackend` trait so they are testable without hardware. | No (delegates to `headset-device`) |
| `headset-tray` | `headset-tray.exe`. The Windows tray application: a safe state model, a device thread owning the one `ControlSession`, and a confined `unsafe` Win32 module for the notification icon, menus, and Core Audio microphone mute. | Yes — see Unsafe confinement |

## `HidBackend` / `HidTransport` / `OpenMode`

These are the trait and type signatures `headset-device` implements, as designed for
Task 3:

```rust
pub trait HidTransport {
    /// Reads one input report. `buf` must be at least the collection's
    /// `input_report_len`. Returns the number of bytes read, including the
    /// leading report-ID byte.
    fn read_report(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, DeviceError>;

    /// The collection's declared input report length, including the report-ID byte.
    fn input_report_len(&self) -> u16;
}

pub trait HidBackend {
    fn enumerate(&self) -> Result<Vec<CollectionInfo>, DeviceError>;

    fn open(&self, id: &DeviceId, mode: OpenMode)
        -> Result<Box<dyn HidTransport>, DeviceError>;
}

pub enum OpenMode {
    /// CreateFile with dwDesiredAccess = 0. Descriptor queries only.
    /// Cannot perform I/O, therefore cannot contend with the audio stack.
    Descriptors,
    /// FILE_GENERIC_READ only. Can receive input reports; cannot write.
    Read,
    /// FILE_GENERIC_READ | FILE_GENERIC_WRITE. Granted only to the control
    /// session, and only after the descriptor-shape gate has passed.
    ReadWrite,
}
```

Phase 2 added `write_report` and `output_report_len` to `HidTransport`. Both are
**defaulted**: `write_report` returns `DeviceError::WriteNotSupported` unless a transport
overrides it. A read-only handle therefore cannot acquire write ability by inheriting an
implementation, which makes the guarantee a property of the type rather than of caller
discipline. `WindowsTransport` overrides it only when constructed from a handle opened
with `OpenMode::ReadWrite`.

`probe` uses `OpenMode::Read`, so its documented read-only contract is now enforced by
the access rights Windows granted, not merely by the absence of a call.

## Device profiles and codecs

A product is supported exactly when it has a `DeviceProfile` in
`crates/headset-device/src/profile.rs`. A profile carries everything the rest of the
workspace may assume about that product:

- its vendor and product id, which is how it is recognised;
- the `ControlShape` its protocol was measured on — the control collection's usage page
  and report lengths;
- the `Codec` that encodes and parses its reports.

`Codec` (in `headset-protocol`) is the seam between devices. It maps the shared `Param`
vocabulary to that device's identifiers and ranges, holds its read and write allowlists and
any write preamble the vendor software was observed sending, and parses one input report
into the `ParamFrame`s it carries — zero or one for the V3 Pro. A device with no evidence
for a parameter does not map it, which is how "this device has no such setting" is
expressed; nothing reaches an identifier by assuming two devices share one.

`ControlSession` keeps the profile it resolved, and callers encode through
`session.codec()` and interpret frames through the same codec. The CLI and the tray never
name a device's identifiers directly.

Adding a product means adding a profile and, when its protocol differs, a codec module —
each backed by its own capture evidence in `docs/`. The BlackShark V3 Pro (`0x1532` /
`0x101B`, codec `v3pro`) is currently the only profile.

## Control device resolution

`ControlSession` never resolves the device by candidate index or by a stored device path.
Both are machine-local: the control collection sits at a different absolute index on a
different PC, and the path embeds an instance id that changes when the dongle is
replugged. Resolution runs on every session: enumerate, keep collections whose product has
a profile, and require the usage page that profile's protocol was measured on, then check
the report lengths against the same profile. Zero matches and more than one match are both
errors rather than a guess.

The descriptor-shape check is a safety gate, not a sanity check. Each profile's protocol
was derived from that product's measured framing; a control collection that differs from
its own profile's shape is refused rather than written to on the assumption that the
framing applies.

## Candidate selection and the supported-device allowlist

`crates/headset-device/src/select.rs` provides two independent pieces of logic that
`headset-cli`'s commands compose:

- `rank_candidates` scores every enumerated collection purely from descriptor shape
  (vendor-defined usage page, declared report width, bidirectionality) and disqualifies
  collections that can never be a control channel (the Windows audio-stack collection,
  and anything with no writable report path). This scoring has no idea what device it is
  looking at — it will happily rank an unrelated vendor's HID collection above every
  collection belonging to the headset this project actually supports, if that unrelated
  collection happens to declare a wider report.
- `is_supported_device` positively identifies the products this project has a profile for
  (`PROFILES` in `profile.rs`; currently only `0x1532` / `0x101B`).

**Automatic candidate selection must always be scoped to `is_supported_device`, never to
shape-based ranking alone.** `headsetctl probe`'s default (no `--candidate`) path applies
this scope, and `headsetctl list`'s "Best control candidate" line applies the same scope
to the collections it is showing. This is a safety-relevant rule, not a style
preference: without it, the highest-scoring collection on the machine — which can belong
to any vendor's HID device, not just the headset — gets automatically selected as the
control candidate. This exact failure mode was found and fixed twice: once in `probe`
(Task 10 fix round) and once in `list` (Task 11 fix round, `docs/device-research.md` /
the fix-round ledger), where `list` printed `Best control candidate: index 14` for an
MSI motherboard EC controller instead of the headset. `list` continues to show every
collection it enumerates, unfiltered — the allowlist affects only which one, if any, is
labelled the control candidate. An explicit `--candidate <index>` bypasses the allowlist
deliberately (it is the documented escape hatch for investigating other hardware) and is
flagged with `supported_device: false` plus a stderr warning when it does.

## Unsafe confinement

`headset-protocol` declares `#![forbid(unsafe_code)]`: it has zero operating-system
access and needs none.

`unsafe` is confined to the workspace's two Win32 areas:

- `crates/headset-device/src/windows/ffi.rs` — raw SetupAPI and `hid.dll`, including
  `HidD_SetOutputReport` for the write path.
- `crates/headset-tray/src/win32/` — the notification icon, popup menus, message loop,
  process check, and Core Audio. Within it, `audio.rs` is the only place in the workspace
  that calls an interface Windows does not document; it is set apart precisely so that
  exception is one file rather than a line buried in the message loop. See
  [`undocumented-apis.md`](undocumented-apis.md).

The second one is a deliberate Phase 2 trade. Phase 1 stated that `headset-tray` would be
entirely safe Rust, which assumed it would take a tray-icon crate. Phase 2's footprint
rule is zero new crate dependencies — replacing a heavyweight vendor application with
another heavyweight application defeats the purpose — so the tray calls `Shell_NotifyIcon`
and Core Audio directly instead. A second confined `unsafe` module was judged the better
side of that trade. Every other module in every crate is safe Rust, and `headset-cli`
contains none at all.

## Switching the output device

The tray can move Windows' sound to another device while the headset is powered down, and
back when it returns. Two things are worth knowing about how it is built:

- **The trigger is the link parameter, not a button.** No power-button event exists in the
  observed protocol, and none is invented. Holding the power button powers the headset
  off, and *that* is what parameter `0x20` reports. The link carries no reason, so an
  auto-sleep and a walk out of range look identical to a deliberate power-off — which is
  why the switch is debounced rather than immediate.
- **The way back is a persisted debt, not a remembered device.** When the sound is moved,
  the endpoint it was moved *from* is written to the registry. Its presence is the state:
  it means "we owe this user a move back". Holding it in memory would strand somebody on
  the speakers if the tray were closed, updated, or killed while the headset was off.
- **A debt that cannot be paid is not a reason to refuse new work.** Endpoint ids are
  durable but not permanent: a reinstalled driver or a re-enumerated dongle retires one and
  issues another. Alpha 2 read the debt's mere *presence* as "already switched", so a debt
  naming a retired endpoint could never be discharged (the endpoint was gone) and never be
  replaced (its presence blocked the switch) — the feature died silently and stayed dead
  until somebody edited the registry. A debt now counts only while the machine still has
  the endpoint it names, and one whose endpoint never reappears is given up on after
  `output::RESTORE_ATTEMPTS` looks.

The decision itself lives in `headset-tray/src/output.rs` and touches nothing: it is handed
facts and answers with an `Action`. That split exists because the alternative is a rule
whose only test is powering a headset off and watching a machine's sound move.
`win32::plan_output` gathers the facts, `win32::reconcile_output` carries the action out,
and `headset-tray.exe --explain-output` prints what would happen for either link state
without doing it.

Failures are surfaced rather than logged into a subscriber nobody enabled: a balloon from
the tray icon the first time a problem appears, and the same problem persisted — by name,
not by its wording — so the settings row can still explain itself hours later, on the row
of the feature that earned it.

The same module carries a second, independent rule: **the game/chat split**. Windows keeps
three defaults (console, multimedia, communications) and the headset presents two endpoints,
which is the whole reason a chat channel exists. Restoring the sound can only name the one
endpoint it was moved from, and writing that into all three roles is actively destructive —
it overwrites a communications default the user had pointed at the chat channel. So when
`Slot::Game` and `Slot::Chat` have both been chosen and the split is on, a headset coming
back produces `Action::Split` instead of `Action::MoveBack`: ordinary playback to the game
channel, calls to the chat channel, and the debt discharged, since the sound has been placed
deliberately. It is opt-in and works with the move-when-off switch turned off, because
wanting calls on their own channel and wanting your sound moved when the headset dies are
different wishes.

Both rules share one retry budget, for the same reason: a wireless link comes up seconds
before its audio endpoints do, so "the endpoint I want is not there" is the ordinary first
answer, not a failure.

Nothing here reaches the headset. Setting the default output has no documented API at all;
see [`undocumented-apis.md`](undocumented-apis.md).

Microphone mute lives in the tray's Win32 module rather than in `headset-device` because
it is a USB Audio Class control, not a vendor HID command. The vendor protocol can only
*report* the headset's hardware mute switch; it cannot set mute, and no such command
exists to find. See `docs/device-research.md`.
