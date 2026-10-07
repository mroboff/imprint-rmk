# MoErgo RMK development notes

## Glove80 and Go60 parity

`crates/glove80-rmk` and `crates/go60-rmk` are equally thin board workspaces.
Shared embedded behavior belongs in `crates/moergo-rmk`; a board crate must not
include or reach into its sibling's source tree. Keep their RMK feature sets
aligned, apart from the Go60-only `_no_split_peripheral_battery_service`
flash-budget switch, and run `just parity-check` for changes to shared services.

`just parity-check` runs `cargo check`, which never links, so it cannot see a
board run out of flash. Build the bundles (`just firmware-all`) for anything
that adds code to a shared service; each half's remaining application-partition
slack is printed per half and recorded as `freeBytes` in `dist/*/manifest.json`.

Board-local code is limited to physical wiring, pins/drivers, device identity,
and hardware that has no sibling equivalent. Document intentional capability
differences. Fixes to shared behavior must cover both boards unless the
hardware itself makes that impossible.

The Go60 hardware configuration is transcribed from MoErgo's official
`moergo-sc/zmk` board definitions. It uses 30 LEDs per half and a 40% hardware
output ceiling. Its central omits the host-facing split-peripheral GATT battery
service because that service overflows the application partition; split battery
state remains available to firmware, while Glove80 exposes both halves to BLE
hosts. For the same reason its central builds at `DEFMT_LOG = "error"` while
the Glove80 builds at `info`: the Go60 central carries the Glove80's feature
set plus both trackpads in the same 0xB6000 partition, and the `info` and
`debug` call sites cost more flash than it has spare. Both divergences are
budget, not capability — undo either one and `go60_lh` stops linking.

The current port supports BLE split and the two Cirque Pinnacle trackpads
(RMK's `cirque_pinnacle` driver, carried through the assembly; wiring in
`crates/go60-rmk/src/trackpad.rs`) and automatic BLE/TRRS half-duplex split
switching. Do not release it as a ZMK replacement until the trackpads,
peripheral pointing forwarding, and automatic split switching are qualified
on hardware. Build and validate its independent UF2 bundle with `just
go60-firmware`; the official family IDs are `0x9809B007` (left) and
`0x980AB007` (right).

## The Imprint board crate

`crates/imprint-rmk` is a third thin board workspace, for the wireless Cyboard
Imprint, kept under the same rules as the two MoErgo crates: shared behavior in
`crates/moergo-rmk`, board-local code limited to wiring, drivers and identity.
Its hardware configuration is transcribed from Cyboard's `zmk-keyboards`
module at tag v2026.07. It differs from the MoErgo boards in ways the hardware
dictates: the halves have no wired link (BLE split only), the two PMW3610
trackballs are declared in `keyboard.toml` rather than wired by hand, the
status LED is active-high (`BOARD_STATUS_LED_ACTIVE_LOW`, a board constant the
shared power monitor reads), the battery level comes from VDDH because RMK has
no MAX17048 driver, and both halves flash through the Adafruit nRF52840
bootloader (family `0xADA52840`). Its LED order is a hypothesis from the wired
Imprint until confirmed on hardware. It shares the Go60's flash-budget
switches (`_no_split_peripheral_battery_service`, `DEFMT_LOG = "error"`).
Build and validate its bundle with `just imprint-firmware`; CI publishes it as
`imprint-rmk.zip` beside the Go60's archive.

## Embedded startup latency budget

Treat central startup as a latency-sensitive path with a hard practical
budget. A firmware that is logically correct after initialization can still
watchdog-loop if larger futures, objects, extra processors, state walks, or
eager split convergence make initialization take too long. Do not diagnose a
boot loop as RAM exhaustion from size alone; first distinguish memory pressure
from added initialization work and timing.

- Keep constructors and registered-processor initialization bounded and
  minimal. Avoid full-state scans, bulk encoding, queue draining, blocking
  waits, and eager cross-half synchronization before the normal split and USB
  loops are alive.
- Prefer a valid conservative initial snapshot followed by asynchronous or
  event-driven convergence. Ephemeral state may briefly use defaults after
  boot; it must not delay startup merely to provide immediate full
  consistency.
- Put durable replica recovery and reconnect convergence in ordinary runtime
  tasks with coalescing/retry semantics. Keep latency-sensitive edges, such as
  layer activity, on their own small best-effort path once transport is ready.
- Hardware-qualify changes that enlarge startup futures or add initialization
  work even when host tests and firmware size checks pass. Canary the central
  with a known-good automatic recovery image and verify stable enumeration
  before flashing the peripheral.

## Nested RMK repository

`dependencies/rmk` is an independent Git repository, not ordinary vendored
source. Inspect it explicitly before making changes:

```bash
git status --short --branch
git submodule status
git -C dependencies/rmk status --short --branch
git -C dependencies/rmk log --graph --oneline --decorate -20
```

The submodule tracks `colonelpanic8/rmk`'s `assembled` branch, which is
generated output. Never commit RMK changes onto it, and never commit only a
dirty submodule pointer or assume an uncommitted nested worktree is part of an
outer commit. RMK work goes on a topic branch and reaches this repository only
through a rebuild of the assembly (below).

## The assembled RMK line

`dependencies/rmk` is pinned to `colonelpanic8/rmk`'s `assembled` branch, which
is compiled by [fork-fold](https://github.com/colonelpanic8/fork-fold) from the
stack vendored as the `dependencies/rmk-assembly` submodule here (upstream
`colonelpanic8/rmk-assembly`). That repository's `manifest.toml` is the intent
(upstream `HaoboGu/rmk` `main` as the base, plus an ordered list of
`fork:fold/*` topic branches), `manifest.lock.json` is the fact (the OIDs and
tree hash of the last build), and `resolutions/` plus `patches/` carry the
tracked conflict resolutions and coherence fixups. Read
`dependencies/rmk-assembly/AGENTS.md` before operating on the stack; do not
work from memory of the workflow.

Consequences for work in this repository:

- The `assembled` history is a chain of `fork-fold: merge <branch>` commits. It
  is rewritten on every rebuild, so its commit IDs are not durable — only the
  lock's tree hash is. Do not base branches on it, cherry-pick from it, or
  merge it back into anything.
- Every RMK change belongs on the topic branch that owns it — currently
  `fold/macro-hooks`, `fold/split-reliability`, `fold/lighting-rynk`, and
  `fold/connection-selection`. Topic branches stay minimal diffs against
  upstream `main` so they remain upstreamable. Pick the branch by subject
  matter; if a change fits none of them, add a new topic branch and a manifest
  entry rather than widening an existing one.
- A change that only makes sense because of the full downstream stack is a
  cross-entry incoherence, not a topic commit. It belongs in the owning entry's
  `fixup` patch in the assembly repository.
- Older notes described `origin/master` as the composed line and named specific
  2026-07-21 tips (`6bcf2d94`, `228f9bcd`, `e4976e38`) as baselines. That line
  is superseded; `origin/archive/pre-fork-fold-master` preserves it. Local
  branch names are not an authority — inspect live remote refs and the manifest
  before choosing a base.

The loop for landing an RMK change here is:

1. Commit it on the owning `fold/*` branch in `dependencies/rmk` (or another
   checkout) and push that branch to `colonelpanic8/rmk`.
2. In `dependencies/rmk-assembly`, `fork-fold update <entry>` (or `update`
   alone to bump the base too), then `fork-fold build`, resolving any conflict
   the build stops on per that repository's AGENTS.md.
3. Push the rebuilt `assembled` branch to the fork, commit the assembly's
   manifest, lock, resolutions, and patches together inside
   `dependencies/rmk-assembly`, and push that commit — then commit the updated
   `dependencies/rmk-assembly` submodule pointer here.
4. Fetch in `dependencies/rmk`, check out the new `origin/assembled` tip, and
   move the outer pin as described below.

## Rynk protocol compatibility

- Keep existing postcard layouts and endpoint meanings stable; prefer new
  commands and new types.
- Do not mint `ProtocolVersion` values downstream. Discover downstream support
  through capability bits and/or command probing; older firmware must answer
  `UnknownCmd` safely.
- Regenerate wire values, wire frames, and the generated protocol reference
  for intentional protocol additions, while retaining the upstream-owned
  protocol version established by the normalization commit.

## Moving the outer pin

Before updating `dependencies/rmk` in this repository:

1. Format RMK and run its protocol snapshots, native Rynk tests/doctests,
   relevant `cargo nextest` suites, clippy/no-std checks, and WASM build/type
   checks.
2. Build both Glove80 firmware halves from this repository. Protocol and
   compositor changes require hardware qualification before release.
3. Push the owning `fold/*` branch and the rebuilt `assembled` branch to
   `colonelpanic8/rmk`, and commit the assembly repository's manifest, lock,
   resolutions, and patches. A pin to an unpushed or locally-built `assembled`
   commit is not reproducible.
4. Update the outer gitlink and any generated WASM/provenance artifacts in the
   same logical change. Keep the previous gitlink SHA in history as the
   rollback point; because `assembled` is rewritten on each build, that SHA is
   the only rollback target — recover it from this repository's history, not
   from the fork's reflog.
