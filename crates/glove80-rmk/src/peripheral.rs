#![no_main]
#![no_std]

pub const BOARD_LEDS_PER_HALF: usize = 40;
pub const BOARD_SCENE_CAPACITY: usize = 100;
/// 80% of full scale. MoErgo's own `glove80_lh_defconfig` caps
/// `CONFIG_ZMK_RGB_UNDERGLOW_BRT_MAX` at 80 and warns verbatim: "DO NOT CHANGE
/// CONFIG_ZMK_RGB_UNDERGLOW_BRT_MAX TO ABOVE 80. Configuring BRT_MAX above 80%
/// will draw additional current and can potentially damage your computer.
/// WARRANTY IS VOID IF BRT_MAX SET ABOVE 80." 230 sat above that line.
pub const BOARD_CHANNEL_CEILING: u8 = 204;
/// Treat configured colours as sRGB code values rather than raw LED duty.
/// On, mid-tones land where a display puts them; configurations authored by
/// eye against the old linear path will look different.
pub const BOARD_SRGB_COLOR: bool = true;
pub const BOARD_KEEP_LED_POWER_WHILE_AWAKE: bool = false;
pub const BOARD_KEEP_LED_POWER_WHILE_SUSPENDED: bool = false;
/// The status LED is wired active-low on MoErgo's boards.
pub const BOARD_STATUS_LED_ACTIVE_LOW: bool = true;

#[allow(dead_code)]
#[path = "../../moergo-rmk/src/lighting.rs"]
mod lighting;
#[allow(dead_code)]
#[path = "../../moergo-rmk/src/split_lighting.rs"]
mod split_lighting;

use rmk::macros::rmk_peripheral;

#[rmk_peripheral(id = 0)]
mod keyboard_peripheral {
    /// Render the board-wide declarative model locally and present only the
    /// right half's stable slots to its physical chain.
    #[register_processor(runnable)]
    fn lighting_processor() {
        crate::lighting::init_peripheral(p.SPI3, p.P0_13, p.P0_19)
    }

    /// Render the native priority layer edge without waiting for bulk
    /// application traffic.
    #[register_processor(event)]
    fn fast_layer_lighting() {
        crate::lighting::FastPeripheralLayerLighting
    }

    /// Stage and atomically apply semantic snapshots from the central.
    #[register_processor(runnable)]
    fn lighting_replication() {
        crate::lighting::peripheral_replication()
    }

    #[register_processor(runnable)]
    fn lighting_replication_worker() {
        crate::lighting::peripheral_lighting_worker()
    }

    /// Re-render when this half's own USB/VBUS power changes, and report the
    /// VBUS-derived charge state.
    #[register_processor(runnable)]
    fn lighting_power_monitor() {
        crate::lighting::power_monitor(p.PWM0, p.P0_16)
    }

    /// Feed right-half presses directly to PaletteFx. Left-half presses arrive
    /// through the lighting replication task so spatial effects span both
    /// halves without double-counting local hits.
    #[register_processor(event)]
    fn reactive_key_hits() {
        crate::lighting::ReactiveKeyHits::peripheral()
    }
}

pub fn debug_stamp(stage: u32) {
    let _ = stage;
}

pub fn debug_trace_parts() -> (u32, u32, [u32; 2], [u32; 2]) {
    (0, 0, [0; 2], [0; 2])
}

pub fn debug_panic_loc() -> Option<heapless::String<64>> {
    None
}
