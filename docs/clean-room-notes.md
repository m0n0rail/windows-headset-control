# Clean-Room Notes

## Purpose

Record which public behavioral facts were consulted, and affirm that all implementation
code in this repository was written independently.

## Reference material treated as read-only research

- `https://github.com/RiskRunner0/blackshark-linux` — consulted as a statement that a
  vendor HID control path exists for a headset in this product family, and as the source
  of the initial hypotheses listed below. It was not forked. No source file, function,
  comment, asset, README text, or project structure was copied from it. It does not
  appear to carry an explicit license, so it is treated as all-rights-reserved.

## Hypotheses taken from public discussion, and their verification status

| Hypothesis | Source | Status on our hardware |
| ---------- | ------ | ---------------------- |
| VID `0x1532` | public discussion | Confirmed by our own enumeration |
| PID `0x0577` | public discussion | **Refuted.** Our hardware reports `0x101B` |
| Control on USB interface 5 | public discussion | Consistent, but not distinguishing: interface 5 carries all four collections on this device (both vendor-defined ones and both standard ones), so interface number alone cannot identify the control channel. See `docs/device-research.md`, "Interface number is deliberately not counted as a third leg of this corroboration." |
| 64-byte reports | public discussion | Confirmed for `COL04` by our own `HidP_GetCaps` reading |
| Report ID `0x02` | public discussion | Confirmed for `COL04` by our own `HidP_GetValueCaps` reading |
| Sidetone range 0–15 | public discussion | **Confirmed** on our own hardware (2026-08-02). Moving the onboard wheel end to end produced values that clamped at `0x00` and `0x0F` while the control was still turning, and the write path accepts the same range. Established by observing our own device, not by trusting the source |
| Firmware 1.3.x or newer | public discussion | Unverified |

Every "Confirmed" row above was established by reading Windows descriptors on our own
hardware, not by trusting the source. Facts observed from device behavior, USB
descriptors, and our own test results are recorded as facts. No implementation was
derived from third-party source code.

## Feature parity with the prior art is explicitly not a goal (2026-08-02)

The question "does this have feature parity with the Linux prior art?" was raised and
**deliberately not pursued.** Answering it would have required consulting that project's
feature list, which is a fresh consultation and would have added a second entry to the
section above.

The reasoning is not that a README feature list is dangerous to copy — it is not source,
comments, or command tables. It is that knowing which features someone else chose would
steer what this project goes looking for next, and that forward influence is exactly what
the clean-room posture exists to exclude. The protocol work was captured independently
and is complete; there was no need to import someone else's scope to finish it.

**Consequence:** this project's feature set is defined by its own requirements, not by
comparison. The reference material above remains consulted for the narrow purpose already
recorded, and for nothing else. Nobody should later "check parity" without recording that
consultation here first.

## BlackShark V2 Pro (`0x0555`) research (2026-11-10)

The V2 Pro protocol was established only by capturing Razer Synapse driving the contributor's own
hardware (USBPcap and TShark; see
[`device-research-blackshark-v2-pro.md`](device-research-blackshark-v2-pro.md)). **No new reference
material was consulted.** No third-party source, command table, documentation or feature list for
this device was read, before or during the work. The reference listed above was not re-opened for it.

Two kinds of overlap with the V3 Pro are recorded in the research document, and neither was used as
evidence:

- Parameter numbers that coincide (`0x19`/`0x99`, `0x98` before `0x99`, `0x20`, `0x21`, `0x55`). Each was
  confirmed on the V2 Pro by its own isolated session before being named.
- The role byte convention (`01` reply, `02` event). Confirmed on this device by a power-off event.

## Independence affirmation

All code in `crates/` was written from the specification in
`docs/history/specs/2026-07-31-windows-headset-control-design.md` and from
descriptor data measured on our own hardware.
