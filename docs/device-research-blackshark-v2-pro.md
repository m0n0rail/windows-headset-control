# Device Research: BlackShark V2 Pro (`0x1532` / `0x0555`)

This is the evidence record for the second supported device. It follows the standard set by
[`device-research.md`](device-research.md), which covers the V3 Pro: what was sent, what came
back, and under what conditions. Anything whose meaning has not been established by an isolated
observation on this hardware stays unnamed. The [Unknown bytes policy](device-research.md#unknown-bytes-policy)
applies here unchanged.

**The V3 Pro's protocol is not assumed to carry over.** Several parameter numbers do coincide
between the two devices, and that is noted where it happens. But every row in the parameter table
below was established by a session on *this* device that isolated it, never by analogy.

## Method

- Hardware: one BlackShark V2 Pro and its USB dongle, owned by the contributor.
- Capture: USBPcap 1.5.4.0 on the root hub carrying the dongle, filtered to its device address,
  and decoded with TShark 4.6.9. Isochronous audio and standard descriptor requests were dropped
  before analysis.
- Driver of the traffic: Razer Synapse 4.0.896, which the contributor is licensed to run. Nothing
  was sent to the device by this project during the research.
- 23 capture sessions on 2026-11-10 and 11, each isolating one action (see [Session ledger](#session-ledger)).
- No third-party source, command table or documentation was consulted for this device. See
  [`clean-room-notes.md`](clean-room-notes.md).

Raw captures are not published. Parameter `0x00` returns the device serial (see
[Parameters observed but not identified](#parameters-observed-but-not-identified)), and the
USB string descriptors carry it too.

## Verified hardware baseline

Measured with `HidD_GetAttributes`, `HidP_GetCaps` and `HidP_GetValueCaps` (via `headsetctl list`
and `headsetctl inspect`, descriptor access only) on 2026-11-10.

| Collection | Usage page | Usage | Input len | Output len | Feature len | Report ids |
| --- | --- | --- | --- | --- | --- | --- |
| `COL01` | `0x000C` | `0x0001` | 3 | 0 | 0 | in `0x01` |
| `COL02` | `0xFF00` | `0x0001` | 64 | 64 | 0 | in `0x02`, out `0x02` |

VID `0x1532`, PID `0x0555`, version `0x0001`, product string `Razer BlackShark V2 Pro 2.4`, no
manufacturer string, serial present. Both collections are on USB interface 3. `COL02` declares
one 63-count, 8-bit value item in each direction (input usage `0x0002`, output usage `0x0003`).

There is no telephony (`0x000B`) collection, and only one vendor-defined collection. `COL02` is the
control collection: it's the only collection with a writable report, and every vendor exchange
below happens on it.

## Transport

**Different from the V3 Pro.** Every host-originated vendor message in every session was an
**interrupt OUT** transfer on endpoint `0x02` (64 bytes, report id `0x02`). No `SET_REPORT` was
observed. Replies and device-originated events arrive as **interrupt IN** on endpoint `0x83`
(64 bytes, report id `0x02`).

`COL01`'s consumer-control reports (report id `0x01`) share endpoint `0x83`.

After each vendor write, Synapse also issued a control `GET_REPORT` (`bmRequestType 0xA1`,
`wValue 0x0102`, `wIndex 3`, input report id 2). Its reply began `02 4f` in every case but one,
which began `02 46`. **The meaning of this exchange is unknown.** It isn't needed to read a
parameter's reply, which arrives on endpoint `0x83` either way.

**Synapse does not poll while idle.** Session 01: no host traffic in 30 s with Synapse open.

## Framing

The layout below held for every frame in every session. Field names are given only where their
role was observed directly, from lengths that agreed and echoes that matched across hundreds of
frames.

### Host → device

```text
[0]      02        report id
[1]      80        constant in every host frame
[2]      LEN       byte count from [5] to the end of the message
[3..4]   00 00
[5..6]   50 41     constant in every host frame
[7]      FAMILY    08 for parameter access (the only family this project uses)
[8]      00
[9]      BODYLEN   byte count from [10] on; LEN = BODYLEN + 5 in every frame
[10]     CMD       parameter id; bit 7 set = write, clear = read
[11]     00
[12]     N         payload length: 00 for every read, 01 for one-byte writes, 0A for 0x95
[13..]   payload
[..63]   00
```

Bytes after the payload were `00` in all host frames. **There is no checksum.** The V3 Pro's byte-62
XOR does not apply: byte 62 was `00` in every one of the 139 host frames of session 02, whatever the
content.

A second host family, `0E` (`50 41 0e 00 02 e1 01`), was sent before or after nearly every
parameter request. Its meaning is not established and it is not needed to read or write any
parameter below.

### Device → host

An input report holds **one or more concatenated messages**:

```text
[0]      02        report id
[1]      LEN       total byte count of the messages that follow;
                   bytes after [1 + LEN] are left over from an earlier report and are not data
[2..]    message, message, …
```

Each message is `10 + L` bytes:

```text
rel 0..1   50 49     message marker
rel 2      FAMILY    08 parameter access; others listed below
rel 3      SEQ       increments, wrapping from ff to c0
rel 4..7   unknown   changes every message; not decoded
rel 8      L         byte count from rel 10 on
rel 9      00
rel 10..   body
```

For family `08` the body is:

```text
rel 10     CMD       echoes the parameter id
rel 11     ROLE      01 reply to a request | 02 unsolicited event
rel 12     N         value length
rel 13..   value     for a read reply or an event: the parameter's value
                     for a write reply: one status byte (see below)
```

The stale-tail rule was established in session 02: the byte just past the declared length of an
`e1` reply always equalled the value byte of the reply before it. Session 03 confirmed the
concatenation, with reports whose `LEN` was the sum of two messages' lengths.

The role byte has the same convention as the V3 Pro's. It was confirmed on this device by the
power-off event in session 03 (`0x20` with role `02`) against every request reply (role `01`).

Other device families seen, all **not decoded**: `01` (replies to the `e1` request, and about once a
second unsolicited), `0E` (power events, below), `11` (unsolicited, alongside `01`).

### Write replies

Every write was answered with a family `08` reply carrying one status byte. The byte was `00` in
nearly all cases, and `01` in these:

- `0x96` (mic EQ mode): `01` on every write, in every session.
- `0x93` (EQ preset): `01` on the four writes that switched between the standard and esports banks
  (sessions 13, 14, 16); `00` on every write within one bank (sessions 10–12, 15).

In both cases the setting took effect: Synapse's view changed, and `0x13` read back the new value
in session 19. **`01` is therefore not a refusal, and its meaning is unknown.** No write was answered with `FF`,
and Synapse never wrote a value the device could have refused, so how this device reports a
refusal is **unobserved**. (`FF` does occur as a legitimate *value*: `0x13` reads `FF` when the
preset is custom.)

## Parameter table

A name appears only where behaviour was observed to change with the value.

| Id | Read | Write | Event | Name | Values | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `0x20` | yes | — | yes | link state | `01` connected, `00` not | S03: event `00` 0.1 s after power-off; read `01` after power-on. S21: events `00` at power-off and `01` at power-on |
| `0x21` | yes | — | yes | battery level | percent | S03: read `0x37` while Synapse showed 55%. S20: event `0x32` at the moment the UI changed to 50%. See [battery](#battery-0x21) |
| `0x55` | — | — | yes | mic mute (hardware) | `01` muted, `00` not | S19: event `01` on muting at the headset, `00` on unmuting. No write observed |
| `0x18` | — | `0x98` | — | mic monitoring on/off | `01` on, `00` off | S05: off wrote `98 00` alone; on wrote `98 01`, then `99` with the level |
| `0x19` | **not observed** | `0x99` | — | mic monitoring level | `0x00`–`0x0F` | S04: Synapse's 0–100 slider swept down and up, one step at a time, ending on `00` and `0F`. Every `99` write was preceded by a `98` write |
| `0x2C` | yes | `0xAC` | — | power-saving timeout | `0` off; `15`–`60` minutes | S08, S09: the "turns off after (mins) of inactivity on battery" slider, 15–60, wrote 15 … 60 … 15, and `00` when switched off; every write read back as the same value |
| `0x27` | yes | `0xA7` | yes | do not disturb | `01` on, `00` off | S14: read → write → event, both directions; read back `01` after switching on |
| `0x13` | yes | `0x93` | yes | EQ preset | see [EQ](#playback-eq) | S10, S13, S15, S19 |
| `0x1D` | yes | `0x9D` | — | EQ bank | `01` standard, `02` esports | S13, S15, S19 |
| `0x15` | — | `0x95` | — | EQ band values | 10 signed bytes, `−5`…`+5` | S11, S12 |
| `0x16` | — | `0x96` | — | mic EQ mode | see [mic EQ](#mic-eq) | S16 |

**`0x19` has never been read.** Synapse writes the mic monitoring level but never asked for it in
any session, including startup and reconnect. This project therefore has no observed way to verify a
level after setting it. A caller has to report it as unverified rather than invent a read.

### Battery (`0x21`)

**`0x21` is the battery percentage that Synapse displays.** Every value seen, across all captures:

| Time | Source | Value | Synapse showed |
| --- | --- | --- | --- |
| 23:19 | read at Synapse start (S02) | 54 | — |
| ~23:25 | (between captures) | — | 51 |
| 23:29 | read right after power-on (S03) | 55 | 55 |
| 23:30 | event (S04) | 53 | — |
| 23:39 | event (S10) | 52 | — |
| 23:55 | (no change; S20 start) | — | 51 |
| 00:07:57 | event (S20) | 50 | 50, "at about 00:08" |

Session 20 was a single 12-minute continuous capture compared against the UI. The level is pushed as an
event **only when it changes**, and the one change in that window matched the UI to within the user's
reading precision.

The 54-vs-51 gap from session 02 was timing: the user's 51% came from a change pushed while no
capture was running.

**The level reads high right after power-on, and settles within about 40 s.** This was reproduced in
a dedicated power cycle (S21, 2026-11-11):

| Time | Source | Value | Synapse showed |
| --- | --- | --- | --- |
| before | — | — | 50 |
| 00:11:53 | power-off (`e3 00`, event `0x20 = 0`) | — | — |
| 00:12:35 | power-on after 42 s (`e3 01`, event `0x20 = 1`) | — | — |
| 00:12:37 | read | 55 | 55 |
| 00:12:53 | event | 54 | — |
| 00:13:14 | event | 53 | 53, "after a few minutes" |

S03 showed the same shape (55 read at reconnect, 53 a minute later, against a trend near 51–52). Synapse
displays the high value as well, so it's what the device reports, not a decoding error. The cause
isn't observable from the protocol and isn't recorded. A caller should simply show what `0x21`
reports, and expect it to settle downward after a reconnect.

### Playback EQ

A preset change is always the sequence: read `0x1e`, write `0x93 <preset>`, write `0x9d <bank>`, and,
for custom and esports presets only, write `0x95 <ten bands>`.

| Bank (`0x9d`) | Preset (`0x93`) | Synapse name | Curve sent? |
| --- | --- | --- | --- |
| `01` standard | `07` | Game | no |
| `01` standard | `08` | Music | no |
| `01` standard | `09` | Movie | no |
| `01` standard | `FF` | Custom | yes, the user's ten bands |
| `02` esports | `FA` | Apex Legends | yes |
| `02` esports | `FB` | CS:GO | yes |
| `02` esports | `FC` | Valorant | yes |
| `02` esports | `FD` | Fortnite | yes |
| `02` esports | `FE` | Call of Duty | yes |

- **The standard presets are an id only** (S10), so their curves are on the device.
- **The esports curves are sent by the host.** Synapse uploads all five (`93 fa`…`fe`, each with
  `9d 02` and `95`) at startup and on reconnect (S02, S03), and re-sends the selected one on every
  selection (S15). The curves were identical byte for byte in all three sessions. This project does
  not reproduce them: they are Synapse's content, and any esports support would send them only as
  observed.
- **The esports names map to slots by click order**, as reported by the user in S15. The slots are
  not in Synapse's display order.
- **The headset can cycle presets itself.** S19: a physical control produced `0x13` events `07`, `09`,
  `08`, `FF` in turn, and Synapse answered each with reads of `0x1d` (→ `01`) and `0x13` (→ the new
  preset). These are the observed reads that make the EQ verifiable after a write.

**Bands (`0x95`):** ten signed bytes (two's complement), band 1 first, the leftmost slider in Synapse.
S11 and S12 put `+5` (`05`), `−5` (`FB`), `+3` and `−1` on the wire in the expected positions, matching
the UI's ±5 range at both ends. Synapse writes bands **only when a slider is released**, and always
sends all ten.

`0x1e` was read before every preset change and returned `00` in sessions 10–16. Some reads at startup
and reconnect (S02, S03) returned `01`. Its meaning is unknown, and so is its relation to the
unidentified write `0x9e`.

### Mic EQ

`0x96` alone: `00` default, `01` mic boost, `02` broadcast, `03` conference, `FF` custom (S16). Choosing
custom sends no band data, and **moving a mic custom band sends nothing to the device** (S17: each
release re-sent only `96 ff`), even though the UI shows ±12 dB. The mic custom curve is applied on the
PC and is out of scope.

### Power events, family `0E`

On power-off, an `0E` message with body `e3 00` arrived about 0.1 s before the `0x20 = 00` event. On
power-on, `e3 01` arrived before Synapse re-read everything (S03). Recorded as observed, not named.
`0x20` is the parameter to use for link state.

## Parameters observed but not identified

These were seen on the wire. None is named, and none may be assumed to mean anything:

- **Reads:** `0x00` (15-byte ASCII string with the shape of the device serial: **never log or publish
  it**), `0x01` (`00`), `0x02` (24 bytes, beginning `02 06 0b 00 02 06 0b 00`), `0x1e` (see above),
  `0x2a` (`00`; also pushed as an event).
- **Writes:** `0x9e` (`01`, `00` at startup and reconnect).
- **Other families:** `e1` requests (family `0E`), the `GET_REPORT` exchange, and device families `01`,
  `0E` and `11` beyond the power event.

## Out of scope: not in the vendor protocol

Established by observing that the action produced **no** vendor traffic:

| Synapse control | What it does instead | Session |
| --- | --- | --- |
| Mic volume | UAC `SET_CUR` volume (`wValue 0x0200`), entity 2, interface 0, signed 1/256 dB, −33.5 to 0 dB | S06 |
| Microphone on/off | UAC `SET_CUR` mute (`wValue 0x0100`), entity 2, interface 0 | S07 |
| Volume dial on the headset | `COL01` consumer-control reports; Windows then sets UAC volume on entity 5, interface 0 | S19 |
| Mic custom EQ bands | nothing reaches the device | S17 |
| THX Spatial, bass boost, sound normalization, voice clarity, voice gate, volume normalization, vocal clarity, mic noise cancellation, **including each one's strength slider** (bass boost, sound normalization and voice clarity sliders; the 5-step voice gate slider, Low–High, shown at 3 (Medium) when switched on; the Low–High sliders of the three mic enhancements, shown at 55 when switched on; each slider appears only once its switch is on) | nothing reaches the device; processed on the PC | S18 (switches), S22 and S23 (sliders) |

## Session ledger

| # | Action (one per session) | Established |
| --- | --- | --- |
| 01 | Idle 30 s, Synapse open | Synapse doesn't poll |
| 02 | Synapse fully restarted | Framing, startup reads and writes, esports upload |
| 03 | Headset power cycle | `0x20`, `0x21` point, power events, concatenated messages, role byte |
| 04 | Mic monitoring level 92 → 0 → 100 → 92 | `0x99` range, `0x98` before `0x99`, `0x21` event |
| 05 | Mic monitoring off → on | `0x98` |
| 06 | Mic volume sweep | Out of scope (UAC) |
| 07 | Microphone off → on | Out of scope (UAC mute) |
| 08 | Power saving on → off | `0x2c` read-before-write |
| 09 | Power-saving timer 15 → 60 → 15 | `0x2c` = minutes |
| 10 | EQ mode custom → game → movie → music → custom | Standard presets |
| 11 | Custom EQ band 1 top → bottom → 0 | `0x95`, released values only |
| 12 | Band 1 −5, band 10 +3, both back to 0 | `0x95` sign and order |
| 13 | Standard → esports → standard | `0x9d` bank, esports curve sent by host |
| 14 | Do not disturb on → off | `0x27` |
| 15 | Each esports preset in turn | Esports slots |
| 16 | Mic EQ mode through all five | `0x96` |
| 17 | Mic custom band top → bottom → 0 | Mic bands not sent |
| 18 | Eight enhancements, each on → off | Out of scope |
| 19 | Physical controls | `0x55`, volume dial, `0x13` events and reads, `0x1d` read |
| 20 | 12 min continuous, UI % noted by the user | `0x21` events match the UI; events only on change |
| 21 | Power cycle (off 42 s), UI % noted before, at reconnect and after | `0x21` reads high at reconnect and settles; `0x20 = 1` event on power-on |
| 22 | Every enhancement slider swept end to end, released at each end | Out of scope: no vendor or UAC traffic in 82 s |
| 23 | Mic section only: voice gate, volume normalization, vocal clarity and mic noise cancellation each switched on and its slider moved | Repeats S22: no vendor or UAC traffic |
