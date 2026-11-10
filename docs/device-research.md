# Device Research

This document is the evidence record for the **BlackShark V3 Pro** (`0x1532` / `0x101B`). The
BlackShark V2 Pro (`0x1532` / `0x0555`) speaks a different transport and framing, and has its own
record: [`device-research-blackshark-v2-pro.md`](device-research-blackshark-v2-pro.md). Nothing in
one document is evidence for the other device.

## Verified Hardware Baseline

Measured on our own hardware with `HidP_GetCaps` and `HidP_GetValueCaps` (development
machine, 2026-08-01, read-only spike). Treat as ground truth; re-verify if hardware
changes.

| Collection | Usage page | Usage    | Input len | Output len | Feature len | In report ID | Out report ID |
| ---------- | ---------- | -------- | --------- | ---------- | ----------- | ------------ | ------------- |
| `COL01`    | `0x000C`   | `0x0001` | 2         | 0          | 0           | `0x0C`       | —              |
| `COL02`    | `0xFF13`   | `0x0001` | 62        | 62         | 0           | `0x07`       | `0x06`         |
| `COL03`    | `0x000B`   | `0x0005` | 2         | 2          | 0           | `0x05`       | `0x05`         |
| `COL04`    | `0xFF14`   | `0x0001` | 64        | 64         | 0           | `0x02`       | `0x02`         |

VID `0x1532`, PID `0x101B`, version `0x0100`, product string `BlackShark V3 Pro PS HID`,
manufacturer `Razer Inc`, serial present on all four. All four collections are exposed
on USB interface 5.

**Feature report length is 0 on every collection**, so the control transport is interrupt
output plus interrupt input. `COL04` is the presumptive control collection.

This confirms the live-hardware run recorded in `list --vendor-id 0x1532`
(recorded during the Phase 1 enumeration work), which
enumerated the same four collections at absolute indices 10–13 with identical usage
pages, report widths, and report IDs:

| index | usage page | usage | in/out/feature | report ids | ranking outcome |
| --- | --- | --- | --- | --- | --- |
| 10 | `0x000c` | `0x0001` | 2 / 0 / 0 | in `0x0c` | excluded — not vendor-defined |
| 11 | `0xff13` | `0x0001` | 62 / 62 / 0 | in `0x07`, out `0x06` | score 172 |
| 12 | `0x000b` | `0x0005` | 2 / 2 / 0 | in/out `0x05` | excluded — reserved for Windows audio |
| 13 | `0xff14` | `0x0001` | 64 / 64 / 0 | in/out `0x02` | score 174 — best candidate |

All four descriptor reads were taken with Windows' own descriptor APIs
(`HidD_GetAttributes`, `HidP_GetCaps`, `HidP_GetValueCaps`) against our own attached
hardware. Nothing in this table was taken from, or checked against, any third party.

## Control collection selection

**`COL04` (usage page `0xFF14`) is selected as the control collection.**

Evidence:

- `COL04`'s usage page, `0xFF14`, is vendor-defined (`>= 0xFF00`), so `headsetctl`'s
  ranking (`crates/headset-device/src/select.rs::rank_candidates`) admits it as a
  candidate at all.
- `COL04` declares 64-byte input and 64-byte output reports — the widest writable
  report path of any vendor-defined collection on this device.
- `COL04` declares report ID `0x02` on **both** the input and output direction, so a
  request written with ID `0x02` and a response read with ID `0x02` refer to the same
  logical channel.
- Under the ranking formula (100 for a vendor-defined usage page, `+` the declared
  report width in bytes, `+10` if the collection is bidirectional), `COL04` scores
  100 + 64 + 10 = **174**, the highest of any candidate on this device.

`COL02` (usage page `0xFF13`, 62-byte input and output, but with **asymmetric** report
IDs — `0x07` in, `0x06` out) is a qualified alternative. It also scores above zero
(100 + 62 + 10 = **172**) and remains a legitimate vendor channel by the same criteria.
It loses to `COL04` only on the margin above (172 vs. 174: two fewer report bytes), not
because it is disqualified. It remains reachable through `headsetctl probe --candidate
<index>` for anyone who wants to investigate it instead of, or in addition to, `COL04`.

`headsetctl list --vendor-id 0x1532` on real hardware reports `has_unambiguous_winner`
as true for this device: `COL04`'s score (174) strictly exceeds the next-best qualified
candidate's score (172), so the ranking does not need to guess between two tied
channels here. (`has_unambiguous_winner` would return `false`, and `headsetctl` would
refuse to pick automatically, only if two qualified candidates tied on score — that is
not the case on this hardware, but the code path exists for hardware where it is.)

`COL01` (`0x000C`, keyboard/consumer-control usage page) and `COL03` (`0x000B`,
telephony/headset usage page, usage `0x0005`) are excluded outright, not merely
outscored:

- `COL01` is excluded because `0x000C` is a standard (non-vendor) usage page — it is
  the consumer-control page used for media keys, not a proprietary channel.
- `COL03` is excluded because usage page `0x000B` / usage `0x0005` is the telephony
  headset collection that the Windows audio stack itself binds to. This project's
  global constraint forbids ever opening it in `ReadWrite` mode, so it can never be a
  control candidate regardless of its descriptor shape.

## Transport

**Feature report length is `0` on every one of the four collections**, on `COL04`
specifically and on all others. This was read directly from `HIDP_CAPS`'s
`FeatureReportByteLength` field via `HidP_GetCaps`. A length of zero is not "feature
reports are unlikely" — the Windows HID stack reports that no feature report is
declared for this device at all, so `HidD_GetFeature` / `HidD_SetFeature` have no
report to operate on here.

Therefore the only viable control transport for this device is **interrupt output
paired with interrupt input**: a request is written to the collection's output report
(report ID `0x02`, 64 bytes total) and a response, if any, arrives as an unsolicited or
correlated input report (also report ID `0x02`, 64 bytes total) on the same collection.

Report ID `0x02` occupies byte 0 of the 64-byte buffer in both directions (this is how
HID report-ID framing works: the leading byte of a report buffer carries the report ID
when a device declares more than one ID on that report type). That leaves **63 payload
bytes** per report for whatever request or response structure the device actually
uses — a structure that is currently unknown (see Hypothesis register and Unknown
bytes policy below).

This transport conclusion — feature reports unavailable, interrupt in/out only, 1 ID
byte + 63 payload bytes — was determined entirely from our own descriptor reads. No
part of it was taken from, or needed to be checked against, prior art.

### Correction (2026-08-02): the write path is SET_REPORT, not interrupt output

The paragraph above concluded that interrupt output was the *only* viable transport.
The descriptor facts behind it are unchanged and still correct — feature report length
really is `0` on every collection — but the conclusion drawn from them was too narrow.
Observation of the vendor software (route 1, below) shows it writes with a **control-pipe
`SET_REPORT`**, not an interrupt output transfer:

```
bmRequestType 0x21   (host-to-device, class, interface)
bRequest      9      (SET_REPORT)
wValue        0x0202 (report type 0x02 = Output, report ID 0x02)
wIndex        5      (interface 5)
wLength       64
```

This is the transfer `HidD_SetOutputReport` produces. Responses and unsolicited events
arrive as interrupt input reports on endpoint `0x84`, as originally described. "Feature
reports are unavailable" and "output reports must therefore go out over the interrupt
endpoint" are separate claims; the first is measured, the second was an inference, and
the inference was wrong. The write path implemented in Phase 2 uses
`HidD_SetOutputReport` because that is what was observed working against this hardware.

## Hypothesis register

Updated 2026-08-02, after route 1 was taken (see Blocker, below). Rows that were
`unverified` because no request/response pair had ever been observed are now backed by
captured traffic. Rows still lacking evidence remain `unverified` and are not filled in.

| Hypothesis | Request byte layout | Expected response length | Checksum | Confidence | Evidence |
| --- | --- | --- | --- | --- | --- |
| `COL04` (usage page `0xFF14`, report ID `0x02`) is the control channel | n/a | n/a | n/a | **confirmed** (descriptor and protocol level) | `HidP_GetCaps` on our hardware; and every observed exchange targets report ID `0x02`, 64 bytes, interface 5 |
| Feature reports are unavailable on this device | n/a | n/a | n/a | **confirmed** | `feature_report_len == 0` on all four collections |
| A byte-0 report-ID framing byte precedes 63 payload bytes | byte 0 = `0x02` | n/a | n/a | **confirmed** | Declared report length 64 including the ID byte |
| Writes are delivered by `SET_REPORT` on the control pipe | `bmRequestType 0x21`, `bRequest 9`, `wValue 0x0202`, `wIndex 5` | 64 bytes on interrupt IN `0x84` | n/a | **confirmed** | Observed for every host-originated frame; see the Transport correction above |
| Checksum is `XOR` of bytes 0..61, stored at byte 62 | n/a | n/a | byte 62 = XOR(bytes 0..61); byte 63 reserved | **confirmed** | Verified against all 200+ captured reports, both directions, no exceptions |
| `data_size` at byte 6 equals `4 + payload length` | n/a | 4 + payload | n/a | **confirmed** | Holds across payload lengths 0, 1, 2, 6, 10, and 11 |
| Bytes 9-10 carry the command; bit 7 of byte 10 selects write vs read | bytes 9-10 = command, byte 11 = role, byte 12 = length | varies | as above | **confirmed** | Read/write pairs observed for four parameters: `0x19`/`0x99`, `0x5C`/`0xDC`, `0x6A`/`0xEA`, `0x12`/`0x92` |
| Byte 11 is a role selector (`00` request, `01` response, `02` event) | byte 11 = role | n/a | n/a | **confirmed** | All three values observed with consistent semantics across every capture |
| A response payload of `0xFF` means refused/unavailable, for reads as well as writes | n/a | 1 byte | n/a | **confirmed** | `0x00` on every successful write; `0xFF` from `0x98`/`0x99` writes while the mic was hardware-muted, `0x00` for the identical writes once unmuted; and separately, with the headset powered off, `headsetctl` read `0xFF` from every headset-proxied parameter in one session while `0x20` reported `00 00` |
| Frames with non-zero class/command-id bytes belong to a different family | bytes 7-8 non-zero | varies | as above | **unverified** | Two exchanges observed (`command_id` `0x84` and `0x04`); not decoded, treated as opaque |
| `COL02` (`0xFF13`) carries a usable secondary or legacy control channel | unknown | unknown | unknown | **unverified** | descriptor shape only; no request/response exchanged |

### Parameter table

Parameter id is the low 7 bits of byte 10. A name appears only where behaviour was
observed to change with it; see the Unknown bytes policy.

| Id | Read | Write | Name | Range | Evidence |
| --- | --- | --- | --- | --- | --- |
| `0x20` | `0x0020` | — | link state | 2 bytes | `00 00` while the headset was off; `01 00` pushed at the instant it was powered on, followed by the host reading every other parameter |
| `0x21` | `0x8021` | — | battery level | 0–100 | Read returned `0x34` while the vendor UI showed 52; an event later reported `0x31` after the user observed the level falling |
| `0x19` | `0x8019` | `0x8099` | sidetone level | 0–15 | Slider moved end to end; values clamped at `0x00` and `0x0F` while still turning |
| `0x5C` | `0x805C` | `0x80DC` | game/chat balance | 0–20 | Same clamping evidence at `0x00` and `0x14`; centre `0x0A`. `0x00` is full **game**, `0x14` full **chat** — established by listening, not by the captures, which carry no direction |
| `0x55` | `0x8055` | — | mic mute (hardware switch) | 0/1 | Events on the headset's physical mute; no write ever observed, and the vendor UI exposes no mute control |
| `0x6A` | `0x806A` | `0x80EA` | onboard slider function | ≥3 states | Toggling which parameter the wheel drives changed this value; the wheel then reported through a different parameter |
| `0x12` | `0x8012` | `0x8092` | noise control (mode + ANC level) | 2 bytes; see below | Three capture sessions isolating noise cancellation off/on, mode switching, and level 4→3→2→1→4 |

Writes observed but not otherwise identified: `0x9E` (written `01` during startup).
`0x98` is written `01` immediately before every `0x99` sidetone write, in both
directions of a mute transition, and is not written before any other parameter — it is
recorded as sidetone-adjacent, not as a general preamble.

Reads observed whose meaning is **unknown**, and which are deliberately unnamed:
`0x15` (indexed 0–8, 11 bytes), `0x16`, `0x17` (10 bytes), `0x2A`,
`0x2C`, `0x5D`, `0x5F`, `0x60` (indexed 0–8, 6 bytes), `0x65` (indexed, 2 bytes),
`0x66`. The indexed shape of `0x15`/`0x60` is consistent with a nine-entry table, but
no interpretation of their payload bytes is recorded, and none may be assumed.

### Noise control, parameter `0x12` (2026-08-02, first-party)

`0x12` had been recorded as an observed-but-unidentified two-byte read. Three further
capture sessions, each isolating one action in the vendor UI, identified it.

```text
byte 0   mode    0x00 off | 0x01 ANC | 0x50 ambient
byte 1   level   ANC strength, observed at 0x01, 0x02, 0x03, 0x04
```

Session 1 — noise cancellation off, then on:

```text
OUT 0x8012 REQ  (read)          IN 0x8012 RSP  01 04
OUT 0x8092 REQ  00 04           IN 0x8092 RSP  00
OUT 0x8012 REQ  (read)          IN 0x8012 RSP  00 04
OUT 0x8092 REQ  01 04           IN 0x8092 RSP  00
```

Session 2 — ANC → ambient → ANC. Session 3 — level 4→3→2→1→4, where every write
round-tripped exactly on the following read (`01 03`, then `01 02`, then `01 01`).

Four facts follow, and only these four:

- **Byte 0 is the mode.** Three values were observed. Any other byte is recorded as
  unrecognised rather than being given a meaning.
- **Byte 1 is the ANC level, and it is retained.** Switching off wrote `00 04` while the
  level was 4, and the level was still 4 on the way back on. The vendor UI's four
  positions map to `0x01`–`0x04` directly.
- **Ambient has no level.** The one ambient write carried `50 01` while the level was 4,
  and the device went on reporting `50 04`. Byte 1 is therefore not an ambient level, and
  the `0x01` is reproduced as the constant that was seen rather than treated as one.
  Confirmed independently: the vendor UI exposes no level control in ambient mode.
- **The write is whole-struct.** The vendor software read `0x12` immediately before every
  write and re-sent the byte it was not changing. Writing one byte alone was never
  observed, and would leave the other to whatever the device already held — so
  `encode_write_payload` refuses a one-byte write of `0x12`, and the CLI reads before it
  writes.

The range `1..=4` is **weaker evidence than sidetone's or game/chat's**: those were
established by watching the device clamp at both ends, whereas the vendor UI has exactly
four positions and there was nothing to push past. Values outside it are refused rather
than sent, because what the device does with them is unobserved.

### Reads while the headset is off (2026-08-02, first-party)

The first end-to-end run of `headsetctl` against the hardware happened to catch the
headset powered off, which produced evidence no capture had:

```
link            : 00 00      (not connected)
battery         : ff
sidetone        : ff
game/chat       : ff
mic mute        : ff
slider function : ff
0x2c            : ff
```

Every headset-proxied parameter answered with the same refusal byte the vendor
protocol uses for a rejected write, while `0x20` — the dongle-local link parameter —
answered normally. This is what distinguishes the two address spaces in practice: the
dongle answers for itself and refuses on the headset's behalf when it cannot reach it.

Two consequences for any caller:

- `0xFF` from a named parameter must not be surfaced as a value. 255 is not a battery
  percentage, a sidetone level, or a mute state.
- The refusal is **not** specific to the muted-mic case recorded above. Treat `0xFF` as
  "refused", and consult `0x20` to find out whether an unreachable headset is why.

The raw `param get` path deliberately does **not** apply this interpretation: an
unidentified parameter could legitimately hold `0xFF`, and this project does not guess.

### What is out of scope of this protocol

Two features that a caller might expect to find here are **not** in the vendor
protocol at all, established by observing that changing them produced no vendor
traffic whatsoever:

- **Volume** is USB Audio Class `SET_CUR` on control selector `0x02`, sent to two
  feature units (entity `0x02` on interface 0 and entity `0x0A` on interface 3),
  carrying signed 16-bit values in 1/256 dB.
- **Mic mute, as set by software**, is USB Audio Class `SET_CUR` on control selector
  `0x01`, entity `0x02`, interface 0. It is independent of the hardware mute switch
  reported by parameter `0x55`: toggling the Windows mute produced no `0x55` event, and
  the headset's own switch produced no audio-class transfer. Both were captured in the
  same session.

Anything reading or writing volume or software mute must use the Windows audio APIs.
There is no vendor command to look for.

## Unknown bytes policy

Any byte, bit, or field whose meaning has not been established by direct observation
on our own hardware is recorded as `unknown`, not assigned a plausible-sounding label.
Concretely:

- The 63 payload bytes in every `COL04` request and response are `unknown` until a
  specific byte's role is confirmed by an observed request/response exchange (see
  Blocker, route 1) or equivalent first-party evidence.
- "Unknown" is not a placeholder to be filled with a guess for convenience. A field
  stays `unknown` — in this document, in code comments, and in any parser — until
  there is direct evidence for what it means on this hardware. Prior-art command
  tables for a different PID (see Blocker, below) are not sufficient evidence on
  their own.
- Any future code that parses or constructs `COL04` payloads must treat unknown byte
  ranges as opaque and must not assign field names, offsets, or interpretations to
  them speculatively.

## Blocker: RESOLVED 2026-08-02 via route 1

**Route 1 was taken.** The vendor software's HID traffic was captured with USBPcap and
Wireshark against our own hardware across fifteen sessions, each isolating one action
(one setting changed, one physical control moved, one power cycle). The observed
request/response pairs are recorded above as behavioural facts: what was sent, what came
back, under what conditions.

This is the interoperability method the Blocker section itself named as standard and
clean-room-preserving. It records observed device behaviour. No third-party source,
comment, command table, or documentation was consulted, and the PID `0x0577` prior art
remains unused — its command set was never referenced, and every identifier implemented
in Phase 2 was seen on our own wire.

**Consequence:** sidetone control, game/chat balance control, and noise control are
unblocked. Every command in the allowlist is an identifier observed in these captures —
including `0x92`, whose write bit follows the same bit-7 pattern as the others but which
was added only after the write itself was seen on the wire. Route 3
(assume the other product's command set applies) was **not** taken and remains
prohibited.

The original statement of the Blocker is preserved below, unedited, because the
reasoning that led to it is part of the research record.

---

### Original Blocker text (Phase 1, superseded)

Phase 1B was specified as "exchange one known-safe request and response." **On this
hardware, no such request is known.**

- Our device's PID is `0x101B`. The public Linux prior art
  (`https://github.com/RiskRunner0/blackshark-linux`, see
  `docs/clean-room-notes.md`) targets PID `0x0577` — a different product. Its command
  set cannot be assumed to apply here, and nothing in this repository assumes it does.
- We have no vendor documentation for this device's control protocol.
- Sending a guessed command identifier onto the wire would violate this project's
  standing rule against speculative HID writes (`CONTRIBUTING.md`: "Never send
  speculative or brute-forced HID command identifiers"; global constraint: "This
  entire plan performs zero HID writes").

Therefore Task 10 implements a **passive probe only**: it opens `COL04` (or, via
`--candidate`, an explicitly chosen alternative such as `COL02`) for reading and
listens for unsolicited input reports for a bounded window. It sends nothing to the
device and cannot alter device state. A **silent result — no unsolicited input report
observed — is a legitimate, expected outcome of this probe, not a failure of it.** The
probe's job is to observe, not to provoke a response.

### Routes to a known-safe request

Obtaining an actual known-safe request requires an explicit decision before any HID
write is implemented. None of the three routes below is authorized by this document;
each needs its own explicit go-ahead when it is acted on.

1. **Observe the vendor software.** Capture the HID traffic the manufacturer's own
   configuration software sends to the device (for example with USBPcap plus
   Wireshark) and record the observed request/response pairs here as behavioral facts
   — what was sent, what came back, under what conditions. This is the standard
   interoperability method (observing behavior of software you're allowed to run,
   against hardware you own) and it preserves the clean-room posture described in
   `docs/clean-room-notes.md`, because it records observed device behavior rather than
   copying anyone's source code, comments, or command tables.
2. **Obtain vendor documentation.** Slower, and out of scope for this phase; it
   belongs to the later public-release-readiness phase (see
   `docs/clean-room-notes.md` / a future `docs/manufacturer-contact-draft.md`).
3. **Accept risk on a hypothesis** — i.e., assume the `0x0577` command set (or any
   other guessed byte layout) applies to this `0x101B` device and try it. **Not
   recommended, and not permitted under this project's current rules** without an
   explicit written decision recorded in this document, because it is exactly the
   kind of speculative HID write `CONTRIBUTING.md` prohibits.

Sidetone control, or any other write-capable feature, is **blocked** on one of these
three routes being taken and recorded here. It is not close to working; no request
format is known, and none should be implied to be known or "probably fine to try."

## Probe observation (2026-08-01)

`headsetctl probe` (Task 10) was run against this hardware with audio actively playing
through the headset. Both vendor-defined collections were listened on, read-only:

- `COL04` / `0xFF14` (candidate index 13, report ID `0x02`, 64-byte reports) — **silent**
  at both a 2000 ms and a 5000 ms listen window. No unsolicited input report arrived.
- `COL02` / `0xFF13` (candidate index 11, report IDs `0x07` in / `0x06` out, 62-byte
  reports) — **silent** at a 5000 ms listen window. No unsolicited input report arrived.

This was re-confirmed after a fix to `probe`'s automatic candidate selection (see the
Task 10 fix-round history) through the corrected default path — i.e. bare `headsetctl
probe` genuinely opening `COL04` on its own, not only via an explicit `--candidate`
override.

**What this does and does not mean.** A silent result on both collections is consistent
with a request/response-only control channel: the device may simply not push status
reports unsolicited, and only replies to a request it receives on its output report.
This is exactly the outcome predicted by the Blocker section above, which established
that no known-safe request exists yet to elicit a response. It is **not** evidence that
either collection is the wrong one, that the device is unresponsive, or that anything is
broken — the descriptor-level evidence for `COL04` (widest bidirectional vendor report,
matching report ID both directions) stands on its own regardless of this observation.

This finding does **not** license trying a speculative write. The Blocker section's
three routes to a known-safe request are unchanged, and none of them has been taken.

### Explanation of the silence (2026-08-02)

The probe's silence now has a direct explanation, and the tentative reading above — "the
device may simply not push status reports unsolicited" — is **wrong**. The device does
push unsolicited input reports: battery changes, mic mute transitions, onboard wheel
movement, and headset connect/disconnect all arrive on `COL04` with no request.

It pushes them **only when something changes**. Task 10's probe ran with audio playing
but with no setting altered and no control touched, so there was nothing to report. The
listen window was not too short; the device had nothing to say.

This is worth re-running as a positive control: probing `COL04` while turning the
onboard wheel produces a report per detent. A probe that changes nothing will still,
correctly, observe nothing.

## Prior-art hypotheses: confirmed and refuted

`docs/clean-room-notes.md` records the hypotheses taken from public discussion before
any hardware was measured. Cross-referencing them against the Verified Hardware
Baseline above:

**Confirmed on our hardware:**

- VID `0x1532` — confirmed by our own enumeration.
- 64-byte reports on the control collection — confirmed for `COL04` by our own
  `HidP_GetCaps` reading (64-byte input, 64-byte output).
- Report ID `0x02` on the control collection — confirmed for `COL04` by our own
  `HidP_GetValueCaps` reading (report ID `0x02` declared on both input and output).
- Control functionality on USB interface 5 — consistent; interface 5 carries all four
  vendor-plus-standard collections on this device, including both vendor-defined ones.

**Refuted on our hardware:**

- PID `0x0577` — **refuted**. Our device reports `0x101B`. The public prior art's PID
  hypothesis simply does not hold for this product.

**What the corroboration does, and does not, license us to conclude.**

Report ID `0x02` and the 64-byte report size both matching the public prior art's
hypotheses, *despite* the PID being wrong, is a striking corroboration — two
independent structural details (report ID and report size) lining up with a
different product's publicly discussed behavior is unlikely to be pure coincidence.
It is reasonable to read this as evidence that the BlackShark V3 Pro PS (`0x101B`)
and the product with PID `0x0577` plausibly share a firmware or protocol lineage at
the framing level (report ID, report size).

Interface number is deliberately not counted as a third leg of this corroboration.
Every collection on this device — including `COL01` and `COL03`, both excluded
outright as candidates — sits on interface 5 (see Verified Hardware Baseline,
above), so interface number does not distinguish the control channel from anything
else on this device. It therefore cannot serve as independent evidence that this
device's control framing shares lineage with the `0x0577` product's control
channel; it is merely consistent with the prior-art hypothesis, nothing more.

It does **not** license us to conclude that the *payload* semantics — what any of the
63 unknown bytes inside that report mean, what command values are valid, what a
response looks like, or whether any command from the other product's command set would
even be accepted rather than ignored or misinterpreted — also carry over. Framing
match is not protocol match. Sending a command byte from the `0x0577` command set to
this `0x101B` device on the strength of this corroboration alone would still be
exactly the kind of speculative, unverified HID write this project's rules prohibit
(see Blocker, above). The corroboration is a reason to consider route 1 (observing
vendor software) a promising next step, not a reason to skip straight to a write.
