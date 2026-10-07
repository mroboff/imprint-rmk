# Imprint RMK firmware (experimental)

This board crate is an RMK port for the wireless Cyboard Imprint, built on
the shared MoErgo RMK services in this repository: the two nRF52840
(Assimilator) halves, their 7×8 matrices over a BLE split link, Rynk
control, forty-one per-key RGB LEDs a half with the vendor's 50% output
ceiling enforced in the firmware, and the two PMW3610 trackballs (the
peripheral half's reaches the central over the split link).

The hardware facts come from Cyboard's `zmk-keyboards` module at tag
v2026.07. The LED count and order are the wired Imprint's, carried over as
a hypothesis: Cyboard's ZMK files declare an arbitrary chain length, so the
table in `keyboard.toml` must be confirmed on hardware before colors can be
trusted to land on the right keys. The battery level is read from VDDH; the
board's MAX17048 fuel gauge has no RMK driver yet.

Hardware qualification is required before relying on this image as a
replacement for the supported ZMK firmware. Build both halves from the
repository root with `just imprint-firmware`; the UF2 family is the Adafruit
nRF52840 bootloader's `0xADA52840` for both halves.
