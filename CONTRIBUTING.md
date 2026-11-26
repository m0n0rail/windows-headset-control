# Contributing

Thanks for looking. This is a small, opinionated project: it talks to two headsets over
protocols reconstructed by observation, and most of its rules exist to keep that honest.

## Before you start

**Read [`docs/device-research.md`](docs/device-research.md).** It is the record of what was
observed on the wire and what deliberately remains unnamed. Nearly every rule below follows
from it. [`docs/device-research-blackshark-v2-pro.md`](docs/device-research-blackshark-v2-pro.md)
is the same record for the second device, and shows what adding one takes.

If you are proposing support for a **different device**, open an issue first. It is not a
small change: it needs its own capture evidence, and the answer to "does the protocol carry
over?" is not assumed to be yes. A device is added as a profile in
`crates/headset-device/src/profile.rs` and, where its protocol differs, a codec module in
`headset-protocol`; see [`docs/architecture.md`](docs/architecture.md).

## Building

Windows only. The workspace targets **`x86_64-pc-windows-gnu`**, not the more common MSVC
target, and this is not incidental:

- `rust-toolchain.toml` pins the toolchain and the target, so `rustup` selects both
  automatically when you build inside the repo.
- The `windows` crate is pinned to 0.58 because 0.59+ uses `raw-dylib` linkage, which needs
  `dlltool.exe`; the GNU toolchain used here does not ship one. `Cargo.lock` pins
  `windows-sys`, `windows-targets`, and `console` for the same reason, and
  `.cargo/config.toml` sets `getrandom_backend = "windows_legacy"` to keep the rest of the
  tree off it too.
- **A routine `cargo update` will break the build** by bumping one of those past its pin.
  If you need to update a dependency, check that the whole tree stays off `raw-dylib`
  before proposing it.

Two things follow from that target and neither is obvious from a failed build:

- **You need MinGW-w64 on `PATH`.** The GNU target links with `gcc`, and rustup does not
  supply one. `winget install BrechtSanders.WinLibs.POSIX.MSVCRT` is enough; take the
  MSVCRT runtime variant, which is the C runtime this target links against. Without it the
  build stops at `linker 'gcc' not found`.
- **Make GNU your default host, not just an installed target.** `rustup target add` alone
  is not sufficient: build scripts and proc macros compile for the *host* triple, so a
  stock MSVC-host rustup fails with `linker 'link.exe' not found` before it reaches any of
  this project's code — and installing Visual Studio to satisfy that is the wrong fix.

```powershell
rustup set default-host x86_64-pc-windows-gnu
cargo build --workspace
cargo test --workspace
```

CI gets away without the first step because GitHub's Windows runners ship MinGW-w64
already, and without the second because it passes `--target x86_64-pc-windows-gnu`
explicitly while running on a host that has both linkers.

Most tests need no hardware: the device layer has a fixture-driven fake backend, and the
panel layout, protocol codec, and placement geometry are pure and fully unit-tested. The
tests that do need a headset are `#[ignore]`d *and* gated behind an environment variable,
so they are skipped twice over by default:

```powershell
$env:HEADSET_HARDWARE_TESTS = "1"
cargo test -p headset-device -- --ignored
```

### Installing a local build

`headset-tray.exe --install` copies the executable to `%LOCALAPPDATA%\Programs\HeadsetTray`,
adds a Start menu shortcut, and sets the run-at-sign-in value. It deliberately does **not**
register in Add/Remove Programs — that belongs to the Inno installer, and two things
writing that key is how a stale entry pointing at a deleted file happens.

`--uninstall` reverses exactly that, and leaves an Inno-made installation alone. Remove one
of those through Settings → Installed apps.

To build the setup executable, install
[Inno Setup](https://jrsoftware.org/isinfo.php) and run `.\build-installer.ps1`.

### Changing the icon

The headset is drawn by `ui::icon::icon_pixels`, not stored as an image. If you change it,
regenerate the committed `.ico` and update the pinned pixel counts:

```powershell
cargo run -p headset-tray -- --export-icon crates\headset-tray\assets\headset.ico
```

A test compares the committed file against a fresh generation, so forgetting this fails the
build rather than shipping an icon that disagrees with the tray.

To see the tray panel without a headset:

```powershell
cargo run --release -p headset-tray -- --render-panel .\out
```

That renders every panel state to PNG through the same Direct2D path the live window uses,
which is how appearance changes are reviewed.

## Before every push

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

CI runs exactly this. Clippy warnings are errors here.

## Hard rules

These are not style preferences. A change that breaks one of them will not be merged
regardless of how good the rest of it is.

- **Never copy source, comments, assets, or structure from an unlicensed third-party
  project.** See [`docs/clean-room-notes.md`](docs/clean-room-notes.md) for what has been
  consulted and on what terms. If you consult something new, record it there in the same
  pull request.
- **Never add a HID write without adding it to the allowlist and documenting the
  rationale.** "Rationale" means capture evidence in `docs/device-research.md`: what was
  sent, what came back, under what conditions. Not inference, and not a pattern that holds
  for other identifiers.
- **Never send speculative or brute-forced HID command identifiers.** Not in a branch, not
  behind a flag, not "just to see". The allowlist exists to make this impossible by
  construction; do not route around it.
- **Never commit `.pfx`, `.p12`, private keys, passwords, or signing tokens.**
- **Redact serial numbers and device paths** in issues, logs, pull requests, and commit
  messages.
- **Never publish or change repository visibility without explicit instruction.**

## Style

- Follow the surrounding code. `rustfmt` decides formatting; taste decides the rest.
- **Comments explain why, not what.** The existing code is dense with the reasoning behind
  non-obvious decisions — why the panel holds its bottom edge, why a re-entrancy guard is
  not optional, why a value is refused rather than clamped. Match that.
- **Do not name what has not been observed.** A byte whose meaning is unestablished stays
  unnamed, in code and in documentation. `docs/device-research.md` explains the policy.
- Tests are written first where practical, and named after the behaviour they pin rather
  than the function they call.

## Pull requests

- One concern per pull request.
- Say what you verified and how — including what you could not verify. "Tested on my
  hardware" and "not tested against hardware" are both useful; silence is not.
- Update `docs/` in the same pull request when behaviour or evidence changes.

Contributions are dual licensed under MIT OR Apache-2.0; see the README.
