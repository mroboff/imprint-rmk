#![no_main]
#![no_std]

#[allow(dead_code)]
mod trackpad;

pub const BOARD_LEDS_PER_HALF: usize = 30;
pub const BOARD_SCENE_CAPACITY: usize = 80;
pub const BOARD_CHANNEL_CEILING: u8 = 102;
/// Treat configured colours as sRGB code values rather than raw LED duty.
/// On, mid-tones land where a display puts them; configurations authored by
/// eye against the old linear path will look different.
pub const BOARD_SRGB_COLOR: bool = true;
pub const BOARD_KEEP_LED_POWER_WHILE_AWAKE: bool = true;
pub const BOARD_KEEP_LED_POWER_WHILE_SUSPENDED: bool = true;
/// The status LED is wired active-low on MoErgo's boards.
pub const BOARD_STATUS_LED_ACTIVE_LOW: bool = true;

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
        crate::lighting::init_peripheral(p.SPI3, p.P0_27, p.P1_11)
    }

    /// Render the native priority layer edge without waiting for bulk
    /// application traffic.
    #[register_processor(event)]
    fn fast_layer_lighting() {
        crate::lighting::FastPeripheralLayerLighting
    }

    #[register_processor(runnable)]
    fn trackpad_device() {
        crate::trackpad::init(
            crate::trackpad::RIGHT_DEVICE_ID,
            p.TWISPI1,
            p.P0_19,
            p.P0_21,
            p.P0_22,
            p.P0_25,
            p.P0_23,
        )
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
        crate::lighting::power_monitor(p.PWM0, p.P1_15)
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
    // The reset-cause ring did its job during the crash-loop hunt; all four
    // relay slots now carry wired-link traffic, which is what the debug relay
    // is actually being used to diagnose. Counts that can run into the
    // thousands get a full word; the rest share one, 16 bits each.
    let (stage, boots, _rr, _cause) = crate::panic_store::trace_parts();
    let (rx_bytes, frames_ok, frames_bad, tx_frames, tx_done, err_and_cancel) =
        ::rmk::split::serial::counters::snapshot();
    let half = |v: u32| v.min(0xffff);
    // Lane drops replace selector entries in this slot: a refused key event
    // or diagnostic leaves no other trace, while transport flapping is now
    // visible through the TransportStatus announcements themselves.
    let drops_and_bad = (half(
        ::rmk::split::serial::counters::LANE_DROPS.load(core::sync::atomic::Ordering::Relaxed),
    ) << 16)
        | half(frames_bad);
    let tx = (half(tx_frames) << 16) | half(tx_done);
    let _ = (frames_ok, err_and_cancel);
    let stage_debug =
        crate::split_lighting::STAGE_DEBUG.load(core::sync::atomic::Ordering::Relaxed);
    (stage, boots, [drops_and_bad, tx], [rx_bytes, stage_debug])
}

pub fn debug_panic_loc() -> Option<heapless::String<{ crate::panic_store::REPORT_CAP }>> {
    crate::panic_store::raw_report_loc()
}
