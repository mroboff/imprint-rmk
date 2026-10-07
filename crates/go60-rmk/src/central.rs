#![no_main]
#![no_std]

mod device_data;
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

#[path = "../../moergo-rmk/src/central_lighting.rs"]
mod central_lighting;
#[allow(dead_code)]
#[path = "../../moergo-rmk/src/lighting.rs"]
mod lighting;
#[path = "../../moergo-rmk/src/panic_store.rs"]
mod panic_store;
#[path = "../../moergo-rmk/src/remote_boot.rs"]
mod remote_boot;
#[allow(dead_code)]
#[path = "../../moergo-rmk/src/split_lighting.rs"]
mod split_lighting;
use rmk::macros::rmk_central;

#[rmk_central]
mod keyboard_central {
    #[Overwritten(host_service)]
    fn host_service() {
        use core::fmt::Write as _;

        crate::panic_store::boot_mark();
        crate::panic_store::capture_boot();
        crate::panic_store::stamp(1);

        let dirty = if env!("MOERGO_REPO_GIT_DIRTY") == "1" {
            "-dirty"
        } else {
            ""
        };
        let config_dirty = if env!("MOERGO_CONFIG_GIT_DIRTY") == "1" {
            "-dirty"
        } else {
            ""
        };
        let mut build_label = ::rmk::heapless::String::<128>::new();
        let _ = write!(
            build_label,
            "config {}{} / {} v{} ({}{}) / RMK {}",
            env!("MOERGO_CONFIG_GIT_HASH"),
            config_dirty,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("MOERGO_REPO_GIT_HASH"),
            dirty,
            env!("MOERGO_RMK_GIT_VERSION"),
        );

        ::rmk::host::HostService::new(&keymap, &rmk_config)
            .with_lighting(crate::central_lighting::rynk_controller())
            .with_peripheral_bootloader(crate::central_lighting::route_peripheral_bootloader)
            .with_device_data(
                crate::device_data::descriptor(),
                crate::device_data::record_at,
            )
            .with_build_label(build_label.as_str())
    }

    #[register_processor(runnable)]
    fn lighting_processor() {
        crate::panic_store::stamp(2);
        let mut persisted_scenes = ::rmk::heapless::Vec::<
            ::rmk::types::protocol::rynk::LightingSceneCell,
            { crate::lighting::SCENE_CAPACITY },
        >::new();
        let persisted_policy = storage.read_lighting_scenes(&mut persisted_scenes).await;
        let preferences = crate::lighting::load_preferences(&mut storage).await;
        let mut engine = crate::central_lighting::engine_with_scenes(
            persisted_scenes.as_slice(),
            persisted_policy,
            preferences,
        );
        storage
            .stream_lighting_runtime_conditional_scenes(crate::lighting::SCENE_CAPACITY, |rule| {
                crate::central_lighting::install_rule(&mut engine, rule)
            })
            .await;
        crate::central_lighting::init(&keymap, engine, p.SPI3, p.P0_27, p.P1_11)
    }

    #[register_processor(runnable)]
    fn lighting_rynk_adapter() {
        crate::panic_store::stamp(3);
        crate::central_lighting::rynk_adapter()
    }

    #[register_processor(runnable)]
    fn lighting_replication() {
        crate::panic_store::stamp(4);
        crate::central_lighting::replication()
    }

    #[register_processor(runnable)]
    fn remote_frame_bridge() {
        crate::panic_store::stamp(5);
        crate::central_lighting::remote_frame_bridge()
    }

    #[register_processor(runnable)]
    fn remote_boot_dispatcher() {
        crate::panic_store::stamp(6);
        crate::central_lighting::RemoteBootDispatcher
    }

    #[register_processor(runnable)]
    fn split_transport_lighting_nudge() {
        crate::panic_store::stamp(7);
        crate::central_lighting::SplitTransportLightingNudge
    }

    #[register_processor(runnable)]
    fn trackpad_device() {
        crate::panic_store::stamp(8);
        crate::trackpad::init(
            crate::trackpad::LEFT_DEVICE_ID,
            p.TWISPI1,
            p.P0_19,
            p.P0_21,
            p.P0_22,
            p.P0_25,
            p.P0_23,
        )
    }

    #[register_processor(event)]
    fn left_pointing_processor() {
        crate::trackpad::processor(&keymap, crate::trackpad::LEFT_DEVICE_ID)
    }

    #[register_processor(event)]
    fn right_pointing_processor() {
        crate::trackpad::processor(&keymap, crate::trackpad::RIGHT_DEVICE_ID)
    }

    #[register_processor(event)]
    fn trackpad_layer_modes() {
        // Seeding from storage here is what makes the pads' behavior
        // configuration rather than firmware: nothing about them is decided
        // until this is read back.
        ::rmk::input_device::pointing_config::init(storage.read_pointing_config().await).await;
        ::rmk::input_device::pointing_config::PointingLayerModes
    }

    #[register_processor(event)]
    fn magic_key_actions() {
        crate::remote_boot::MagicKeyActions::new()
    }

    #[register_processor(event)]
    fn battery_lighting_state() {
        crate::central_lighting::BatteryLightingState
    }

    /// Report this half's VBUS-derived charge state; without it no charge
    /// state is ever produced and `charge`-gated lighting rules never fire.
    #[register_processor(runnable)]
    fn lighting_power_monitor() {
        crate::lighting::power_monitor(p.PWM0, p.P1_15)
    }

    #[register_processor(event)]
    fn reactive_key_hits() {
        crate::lighting::ReactiveKeyHits::central()
    }
}

pub fn debug_stamp(stage: u32) {
    crate::panic_store::stamp(stage);
}

pub fn debug_trace_parts() -> (u32, u32, [u32; 2], [u32; 2]) {
    crate::panic_store::trace_parts()
}

pub fn debug_panic_loc() -> Option<heapless::String<{ crate::panic_store::REPORT_CAP }>> {
    crate::panic_store::raw_report_loc()
}
