# moergo-rmk

Shared Glove80 and Go60 firmware plus native control tooling built on
[RMK](https://github.com/HaoboGu/rmk), and, in this fork, the same firmware
for the wireless Cyboard Imprint.

The repository and shared firmware layer are named `moergo-rmk`; board-specific
crates and artifacts retain their Glove80, Go60 and Imprint names. This fork
adds `crates/imprint-rmk` and publishes its bundle as `imprint-rmk.zip`
beside the upstream artifacts; it tracks upstream otherwise.

## Layout

```text
crates/
├── moergo-rmk/       # shared embedded services and parity contract
├── glove80-rmk/      # Glove80 hardware entry points
├── go60-rmk/         # Go60 hardware entry points
├── imprint-rmk/      # Cyboard Imprint hardware entry points (this fork)
├── moergo-config/    # pure runtime configuration model
├── moergo-config-wasm/ # browser adapter for the same model
├── moergo-control/   # multi-board native Rynk CLI
└── xtask/            # repository checks and release packaging
dependencies/
└── rmk/              # pinned upstream RMK/Rynk submodule
```

Each board firmware is a standalone Cargo workspace because it cross-compiles
for the nRF52840. All three compile shared modules from `crates/moergo-rmk/src`
inside their board binaries; no board may include source from another. Native packages share the root workspace. Generated release
artifacts go in `dist/`.

## Setup

```bash
git submodule update --init --recursive
nix develop
```

The development shell provides the pinned Rust toolchain, `just`, Nordic
bindgen support, and native BLE build dependencies.

## Commands

Run `just` inside the development shell to list the supported tasks:

```bash
just fmt       # format every owned Cargo workspace
just check     # validate repository paths and run native checks/tests
just host-test # test the CLI and repository task runner
just board-check  # compile both halves of both boards
just parity-check # host checks plus both-board compilation
just firmware  # build and package both keyboard halves
just dist      # alias of firmware
just go60-firmware # build and package the Go60 images
just firmware-all  # build both release bundles
```

Run the CLI directly with `cargo run -p moergo-control -- --help`. It discovers
and manages either board through Rynk. See
[`crates/moergo-control/README.md`](crates/moergo-control/README.md) for its
commands and [`crates/glove80-rmk/README.md`](crates/glove80-rmk/README.md) for
firmware details.

Boards may register typed, namespaced device data through Rynk. The Go60 uses
this for its automatic split policy, active wired/BLE transport, and cable
detect state; query it with `moergo-control device-data` for JSON output.

## BLE reconnect advertising

Both boards advertise to the host every 30 ms for five seconds, then back off to
200 ms until the existing advertising timeout. A new advertising attempt after
boot, disconnect, profile selection, or waking from advertising-timeout sleep
starts a fresh fast window. Split and dongle advertising timing is unchanged.

The board `keyboard.toml` files expose these compiled settings under `[ble]`:

```toml
advertising_fast_interval_ms = 30
advertising_slow_interval_ms = 200
advertising_fast_timeout_secs = 5
```

Intervals must be 20–10240 ms, with the fast interval no larger than the slow
interval. Set the timeout to zero to use only the slow interval. These are
firmware settings: changing them requires a rebuild and flash, and does not
change the connected keyboard's report rate.

## Board parity

Shared behavior belongs in `crates/moergo-rmk`. A board crate should contain
only hardware entry points, pins/drivers, board-specific device data, and
features that physically do not exist on its sibling. Any intentional
capability difference must be documented; an implementation difference is not
an acceptable reason to duplicate a service.

Every change to shared behavior is checked against both boards. Release
qualification likewise builds both UF2 bundles, even when the initiating bug
was observed on only one model.

## Release artifacts

`just dist` requires a clean repository and the exact clean RMK submodule
revision. It writes both ELF and UF2 images, `SHA256SUMS`, and a provenance
manifest under `dist/`. Packaging validates each half's UF2 family ID and the
application flash range `0x00026000..0x000dc000`.

`just go60-firmware` applies the same validation to the Go60 build and writes
its independent bundle under `dist/go60/`; `just imprint-firmware` does the
same for the Cyboard Imprint under `dist/imprint/`, with the Adafruit
nRF52840 family id `0xADA52840` for both halves. It stages tracked inputs at fixed
build paths and seeds RMK's storage build hash from the source commit, RMK
commit, and platform profile, so identical inputs reproduce identical UF2s.

Release manifests use schema 2 and include canonical configuration and
platform-profile SHA-256 hashes. The platform profile excludes compiled
keymap/behavior defaults and resolves lighting wake layers by name, so a
downstream personal-default build can prove that its hardware, capacities,
event queues, split transport, storage, and lighting topology still match the
stock build. Compare two files with `cargo run -p xtask --
verify-config-profile STOCK CONFIGURED`. Set `MOERGO_DIST_DIR` to package a
comparison build without replacing the normal `dist/` bundle.

Downstream configuration repositories may set `MOERGO_CONFIG_GIT_COMMIT` and
`MOERGO_CONFIG_GIT_DIRTY` to include their source identity in firmware build
labels and release manifests.

A successful build is not hardware qualification. Before release, test both
halves together: typing, layer lighting, state mutation/readback, split
reconnect, USB/BLE transports, sleep/resume, persistence, and bootloader
recovery.
