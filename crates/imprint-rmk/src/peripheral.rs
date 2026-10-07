#![no_main]
#![no_std]

pub const BOARD_LEDS_PER_HALF: usize = 41;
pub const BOARD_SCENE_CAPACITY: usize = 100;
pub const BOARD_CHANNEL_CEILING: u8 = 128;
pub const BOARD_SRGB_COLOR: bool = true;
pub const BOARD_KEEP_LED_POWER_WHILE_AWAKE: bool = false;
pub const BOARD_KEEP_LED_POWER_WHILE_SUSPENDED: bool = false;
pub const BOARD_STATUS_LED_ACTIVE_LOW: bool = false;

#[allow(dead_code)]
#[path = "../../moergo-rmk/src/lighting.rs"]
mod lighting;
#[path = "../../moergo-rmk/src/panic_store.rs"]
mod panic_store;
#[allow(dead_code)]
#[path = "../../moergo-rmk/src/split_lighting.rs"]
mod split_lighting;
use rmk::macros::rmk_peripheral;

#[rmk_peripheral(id = 0)]
mod keyboard_peripheral {
    #[register_processor(runnable)]
    fn lighting_processor() {
        crate::panic_store::boot_mark();
        // Both halves are wired alike: WS2812 data on SPI3 MOSI P0.08, the
        // chain's power rail switched by P1.02.
        crate::lighting::init_peripheral(p.SPI3, p.P0_08, p.P1_02)
    }

    /// Render the native priority layer edge without waiting for bulk
    /// application traffic.
    #[register_processor(event)]
    fn fast_layer_lighting() {
        crate::lighting::FastPeripheralLayerLighting
    }

    #[register_processor(runnable)]
    fn lighting_replication() {
        crate::lighting::peripheral_replication()
    }

    #[register_processor(runnable)]
    fn lighting_replication_worker() {
        crate::lighting::peripheral_lighting_worker()
    }

    #[register_processor(runnable)]
    fn lighting_power_monitor() {
        crate::lighting::power_monitor(p.PWM0, p.P0_30)
    }

    #[register_processor(event)]
    fn reactive_key_hits() {
        crate::lighting::ReactiveKeyHits::peripheral()
    }
}

pub fn debug_stamp(stage: u32) {
    crate::panic_store::stamp(stage);
}

pub fn debug_trace_parts() -> (u32, u32, [u32; 2], [u32; 2]) {
    // No wired link to count on: the relay carries the panic store's reset
    // trace and the lighting replication stage.
    let (stage, boots, rr, cause) = crate::panic_store::trace_parts();
    let stage_debug =
        crate::split_lighting::STAGE_DEBUG.load(core::sync::atomic::Ordering::Relaxed);
    (stage, boots, rr, [cause[0], stage_debug])
}

pub fn debug_panic_loc() -> Option<heapless::String<{ crate::panic_store::REPORT_CAP }>> {
    crate::panic_store::raw_report_loc()
}
