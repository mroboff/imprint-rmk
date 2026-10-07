//! Shared MoErgo LED hardware and half-local standard lighting processors.

use core::cell::{Cell, RefCell};
use core::num::NonZeroU32;

use embassy_nrf::gpio::{Level, Output, OutputDrive, Pin};
use embassy_nrf::peripherals::{PWM0, SPI3};
use embassy_nrf::pwm::{DutyCycle, Prescaler, SimpleConfig, SimplePwm};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::{Peri, bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_time::{Duration, Instant, Timer};
use rmk::core_traits::Runnable;
use rmk::event::{
    EventSubscriber, KeyboardEvent, KeyboardEventPos, LayerChangeEvent, MaintenanceModeEvent,
    SleepStateEvent, SubscribableEvent,
};
use rmk::lighting::topology::MatrixPosition;
use rmk::lighting::{
    BatteryStatusProvider, BuiltinEffect, ConditionalScenes, IndicatorState, LayerState,
    LightingContext, LightingMailbox, LightingOutput, LightingProcessor, LightingService,
    LogicalFrame, Rgb8, SnapshotProvider, StandardCommand, StandardError, StandardLightingEngine,
    StandardReplicaSlot, StandardReply,
};
use rmk::storage::{
    LightingExtensionOverlayRecord, LightingExtensionParamsRecord, LightingExtensionRecord, Storage,
};
use rmk::types::battery::BatteryStatus;
use rmk_palettefx::effects::{CrosshairParams, Effect};
use rmk_palettefx::palette::id as palette_id;
use rmk_palettefx::rmk_lighting::{
    HitQueue, MAX_INITIAL_PARAMS, PaletteFxConfig, PaletteFxSource, TopologyLayout,
};

mod lighting_output;
mod lighting_preferences;

use lighting_output::{ColorProfile, chain_should_power, frame_visible};

/// Board-wide lighting topology for both binaries. `#[rmk_central]` emits
/// `crate::LIGHTING_TOPOLOGY` for the central, but the peripheral macro only
/// emits renderer configuration; the standalone macro reads the same
/// `KEYBOARD_TOML_PATH` and makes identical statics available to both halves.
/// The central binary carries a duplicate flash copy under this namespace,
/// which the nRF52840's 1 MB flash absorbs without contortions.
pub mod topology_config {
    rmk::macros::rmk_lighting_config!();
}

bind_interrupts!(struct Irqs {
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
});

pub const LEDS_PER_HALF: usize = crate::BOARD_LEDS_PER_HALF;
pub const TOTAL_LEDS: usize = LEDS_PER_HALF * 2;
pub const OVERLAY_CAPACITY: usize = 64;
/// Bounds the runtime scene table and, separately, the runtime conditional
/// table, so a host can carry every lighting rule this board would otherwise
/// compile in and still have room to grow. The compiled config currently uses
/// 104 scene cells across seven layers and 65 conditional rules.
///
/// This is a total across all layers, not a per-layer budget: a cell carries
/// its own layer, and every layer shares the one table.
///
/// It is not a cheap constant. Measured on the central binary, each unit of
/// capacity is multiplied across the live tables, atomic-replace staging,
/// replica snapshot in `REPLICA_SLOT`, and the `StandardCommand` payloads
/// queued in a `COMMAND_CAPACITY`-deep mailbox. Both runtime tables intern
/// effects so repeated styles do not pay that multiplier per cell. Raising
/// this value still requires measuring the final central binary.
///
/// Widening a cell costs the same as raising the capacity. Adding the
/// connection and effects predicates to `ConditionSet` grew every conditional
/// cell, and at 160 the central ran out of stack: opening a conditional
/// replace transaction faulted and reset the board. 112 restored the RAM
/// budget the board shipped with before those predicates; the bonded-slot
/// and usb-connected predicates widened the cell again, so 100 gives back
/// that growth with margin. `config/glove80.toml` currently carries 95 scene
/// cells and 47 conditional rules, while `config/go60.toml` carries 73 and 52.
/// Glove80 retains the 100-cell budget; Go60 uses 80 because its larger
/// firmware otherwise faults while opening an atomic table replacement.
pub const SCENE_CAPACITY: usize = crate::BOARD_SCENE_CAPACITY;
pub const COMMAND_CAPACITY: usize = 4;

/// Number of simultaneous key hits each typing-reactive effect can remember.
/// Sixteen covers sustained fast typing on one half.
pub const REACTIVE_HITS: usize = 16;

pub type Engine = StandardLightingEngine<
    'static,
    PaletteFxSource<TopologyLayout<TOTAL_LEDS>, TOTAL_LEDS, REACTIVE_HITS>,
    ConditionalScenes<'static, BuiltinEffect, BoardBatteryProvider>,
    TOTAL_LEDS,
    OVERLAY_CAPACITY,
    SCENE_CAPACITY,
>;
pub type CoreMailbox = LightingMailbox<
    StandardCommand<OVERLAY_CAPACITY, SCENE_CAPACITY>,
    StandardReply,
    StandardError,
    COMMAND_CAPACITY,
>;

pub static CORE_MAILBOX: CoreMailbox = LightingMailbox::new();
pub static REPLICA_SLOT: StandardReplicaSlot<OVERLAY_CAPACITY, SCENE_CAPACITY> =
    StandardReplicaSlot::new();

#[derive(Clone, Copy)]
struct ApplyRequest {
    generation: u8,
    revision: u32,
    digests: rmk::host::ReplicaDigests,
}

#[derive(Clone, Copy)]
enum DiagnosticRequest {
    Status { request_id: u8 },
    FrameChunk { request_id: u8, offset: u16 },
}

static APPLY_REQUESTS: embassy_sync::signal::Signal<rmk::RawMutex, ApplyRequest> =
    embassy_sync::signal::Signal::new();
static DIAGNOSTIC_REQUESTS: embassy_sync::channel::Channel<rmk::RawMutex, DiagnosticRequest, 1> =
    embassy_sync::channel::Channel::new();
static LAST_APPLIED_REVISION: BlockingMutex<rmk::RawMutex, Cell<Option<u32>>> =
    BlockingMutex::new(Cell::new(None));
static LAST_DIGESTS: BlockingMutex<rmk::RawMutex, Cell<Option<rmk::host::ReplicaDigests>>> =
    BlockingMutex::new(Cell::new(None));

/// Pending key-reactive effect hits for this half's own engine instance. Each
/// binary drains its local queue on its next rendered frame. Central-half hits
/// are also mirrored to the peripheral so spatial effects span the seam.
static HIT_QUEUE: HitQueue<REACTIVE_HITS> = HitQueue::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatteryPair {
    pub left: BatteryStatus,
    pub right: BatteryStatus,
}

impl BatteryPair {
    pub const UNAVAILABLE: Self = Self {
        left: BatteryStatus::Unavailable,
        right: BatteryStatus::Unavailable,
    };
}

static BATTERIES: BlockingMutex<rmk::RawMutex, Cell<BatteryPair>> =
    BlockingMutex::new(Cell::new(BatteryPair::UNAVAILABLE));

pub fn battery_statuses() -> BatteryPair {
    BATTERIES.lock(Cell::get)
}

pub fn set_battery_statuses(statuses: BatteryPair) {
    BATTERIES.lock(|current| current.set(statuses));
}

pub fn set_left_battery(status: BatteryStatus) {
    BATTERIES.lock(|current| {
        let mut statuses = current.get();
        statuses.left = status;
        current.set(statuses);
    });
}

pub fn set_right_battery(status: BatteryStatus) {
    BATTERIES.lock(|current| {
        let mut statuses = current.get();
        statuses.right = status;
        current.set(statuses);
    });
}

pub struct BoardBatteryProvider;

pub static BOARD_BATTERIES: BoardBatteryProvider = BoardBatteryProvider;

impl BatteryStatusProvider for BoardBatteryProvider {
    fn battery_status(&self, node: u8) -> BatteryStatus {
        let batteries = battery_statuses();
        match node {
            0 => batteries.left,
            1 => batteries.right,
            _ => BatteryStatus::Unavailable,
        }
    }
}

/// The board-specific hardware-output limit. This remains below every
/// user-controlled transform and protocol path. Scale rather than clamp so
/// RMK's global brightness has no dead zone and RGB ratios remain intact.
const CHANNEL_CEILING: u8 = crate::BOARD_CHANNEL_CEILING;

/// The board-specific transfer function from a requested colour to LED duty.
/// Applied below every user-controlled transform, so global brightness still
/// scales the requested colour and the profile still decodes what survives.
const COLOR_PROFILE: ColorProfile = if crate::BOARD_SRGB_COLOR {
    ColorProfile::SRGB
} else {
    ColorProfile::LINEAR
};
const ONE_FRAME: u8 = 0x70;
const ZERO_FRAME: u8 = 0x40;
const RESET_BYTES: usize = 48;
const ENCODED_LEN: usize = LEDS_PER_HALF * 24 + RESET_BYTES;
const CHAIN_POWER_SETTLE: Duration = Duration::from_millis(120);
/// Rewrite the latched frame once a second even when it has not changed.
///
/// These WS2812 chains are write-only: each pixel holds its colour in a
/// volatile register until the next frame arrives, and nothing reads back.
/// Presentation is otherwise driven by changed-detection, so a frame that
/// stops changing is written once and then never refreshed -- on the Go60
/// right half that left the chain latching noise into visible colour and
/// holding it, since no further write was ever due.
///
/// Only an effect at rest reaches that state. Key-reactive effects
/// (Reactive, Crosshair, and their family) render exact black once every hit
/// has expired, and that constant frame suppresses further presentation
/// indefinitely. Continuously animated effects such as Flow or Rain change
/// every 40 ms tick, so they rewrite the chain constantly and overwrite any
/// corruption before it can be seen: they mask this fault rather than avoid
/// it. One second is well under human patience for a stray pixel while
/// costing one 30-pixel SPI transaction, about 1.6 ms of bus time, per
/// second.
pub(crate) const PRESENT_REFRESH_INTERVAL: NonZeroU32 = NonZeroU32::new(1000).unwrap();
const STATUS_PWM_TOP: u16 = 320;
const STATUS_PWM_DUTY: u16 = 16;
const POWER_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SLEEP_POWER_POLL_INTERVAL: Duration = Duration::from_secs(10);
const ATTESTATION_INTERVAL: Duration = Duration::from_secs(300);

pub(crate) const BOOTLOADER_TAG: u8 = 0xb0;

struct Ws2812Chain {
    spim: Spim<'static>,
    buf: [u8; ENCODED_LEN],
}

impl Ws2812Chain {
    fn new(spi: Peri<'static, SPI3>, data_pin: Peri<'static, impl Pin>) -> Self {
        let mut config = spim::Config::default();
        config.frequency = spim::Frequency::M4;
        config.mode = spim::MODE_0;
        config.orc = 0;
        Self {
            spim: Spim::new_txonly_nosck(spi, Irqs, data_pin, config),
            buf: [0; ENCODED_LEN],
        }
    }

    async fn write(&mut self, frame: &[Rgb8; LEDS_PER_HALF]) -> Result<(), spim::Error> {
        let mut encoded = 0;
        for pixel in frame {
            let pixel = COLOR_PROFILE.apply(*pixel, CHANNEL_CEILING);
            for channel in [pixel.g, pixel.r, pixel.b] {
                for bit in (0..8).rev() {
                    self.buf[encoded] = if channel & (1 << bit) == 0 {
                        ZERO_FRAME
                    } else {
                        ONE_FRAME
                    };
                    encoded += 1;
                }
            }
        }
        self.buf[encoded..].fill(0);
        self.spim.write_from_ram(&self.buf).await
    }
}

pub(crate) struct LightingHardware {
    chain: Ws2812Chain,
}

struct ChainPower {
    pin: Output<'static>,
    powered_at: Option<Instant>,
    frame_visible: bool,
}

// The power monitor must be able to drop a dark chain on USB removal even
// when the unchanged black frame does not require another presentation.
static CHAIN_POWER: BlockingMutex<rmk::RawMutex, RefCell<Option<ChainPower>>> =
    BlockingMutex::new(RefCell::new(None));

fn initialize_chain_power(pin: Output<'static>) {
    CHAIN_POWER.lock(|state| {
        *state.borrow_mut() = Some(ChainPower {
            pin,
            powered_at: None,
            frame_visible: false,
        });
    });
}

fn update_chain_power(usb_powered: bool, sleeping: bool) -> Option<Instant> {
    CHAIN_POWER.lock(|state| {
        let mut state = state.borrow_mut();
        let state = state.as_mut()?;
        let should_power = chain_should_power(
            usb_powered,
            sleeping,
            state.frame_visible,
            crate::BOARD_KEEP_LED_POWER_WHILE_AWAKE,
            crate::BOARD_KEEP_LED_POWER_WHILE_SUSPENDED,
        );
        match (state.powered_at, should_power) {
            (None, true) => {
                state.pin.set_high();
                let powered_at = Instant::now();
                state.powered_at = Some(powered_at);
                Some(powered_at)
            }
            (Some(_), false) => {
                state.pin.set_low();
                state.powered_at = None;
                None
            }
            (powered_at, _) => powered_at,
        }
    })
}

fn set_chain_frame_visible(visible: bool) {
    CHAIN_POWER.lock(|state| {
        state
            .borrow_mut()
            .as_mut()
            .expect("lighting initializes chain power")
            .frame_visible = visible;
    });
}

fn chain_needs_dark_latch() -> bool {
    CHAIN_POWER.lock(|state| {
        state
            .borrow()
            .as_ref()
            .is_some_and(|state| state.powered_at.is_some() && state.frame_visible)
    })
}

fn power_down_chain() {
    CHAIN_POWER.lock(|state| {
        let mut state = state.borrow_mut();
        let state = state.as_mut().expect("lighting initializes chain power");
        state.pin.set_low();
        state.powered_at = None;
    });
}

async fn wait_for_chain_power() -> bool {
    loop {
        let Some(powered_at) = update_chain_power(local_vbus_present(), false) else {
            return false;
        };
        let elapsed = Instant::now().saturating_duration_since(powered_at);
        if elapsed >= CHAIN_POWER_SETTLE {
            return true;
        }
        Timer::after(CHAIN_POWER_SETTLE - elapsed).await;
    }
}

impl LightingHardware {
    pub(crate) fn new(
        spi: Peri<'static, SPI3>,
        data_pin: Peri<'static, impl Pin>,
        chain_power_pin: Peri<'static, impl Pin>,
    ) -> Self {
        initialize_chain_power(Output::new(
            chain_power_pin,
            Level::Low,
            OutputDrive::Standard,
        ));
        Self {
            chain: Ws2812Chain::new(spi, data_pin),
        }
    }

    pub(crate) async fn write(&mut self, frame: &[Rgb8; LEDS_PER_HALF]) -> Result<(), spim::Error> {
        let visible = frame_visible(frame, COLOR_PROFILE, CHANNEL_CEILING);
        if !visible
            && chain_needs_dark_latch()
            && let Err(error) = self.chain.write(frame).await
        {
            power_down_chain();
            return Err(error);
        }
        set_chain_frame_visible(visible);
        if !wait_for_chain_power().await {
            return Ok(());
        }
        match self.chain.write(frame).await {
            Ok(()) => Ok(()),
            Err(error) => {
                power_down_chain();
                Err(error)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum OutputError {
    Spi,
}

/// One physical half's sink for an otherwise board-wide logical frame.
/// Keeping the same board-wide stable slots in both engines avoids a second
/// topology or layer-scene mapping while all animation sampling remains local.
pub struct HalfOutput {
    hardware: LightingHardware,
    first_slot: usize,
}

impl HalfOutput {
    pub(crate) fn left(hardware: LightingHardware) -> Self {
        Self {
            hardware,
            first_slot: 0,
        }
    }

    pub(crate) fn right(hardware: LightingHardware) -> Self {
        Self {
            hardware,
            first_slot: LEDS_PER_HALF,
        }
    }

    async fn present_frame(
        &mut self,
        frame: &LogicalFrame<Rgb8, TOTAL_LEDS>,
    ) -> Result<(), OutputError> {
        let mut local = [Rgb8::BLACK; LEDS_PER_HALF];
        local.copy_from_slice(&frame.as_slice()[self.first_slot..self.first_slot + LEDS_PER_HALF]);
        self.hardware
            .write(&local)
            .await
            .map_err(|_| OutputError::Spi)
    }
}

impl LightingOutput<LogicalFrame<Rgb8, TOTAL_LEDS>> for HalfOutput {
    type Error = OutputError;

    async fn initialize(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn present(&mut self, frame: &LogicalFrame<Rgb8, TOTAL_LEDS>) -> Result<(), Self::Error> {
        self.present_frame(frame).await
    }

    async fn suspend(&mut self) -> Result<(), Self::Error> {
        if crate::BOARD_KEEP_LED_POWER_WHILE_SUSPENDED {
            // The rail stays asserted through suspend, so the chain must be
            // latched dark explicitly before rendering stops.
            return self
                .hardware
                .write(&[Rgb8::BLACK; LEDS_PER_HALF])
                .await
                .map_err(|_| OutputError::Spi);
        }
        set_chain_frame_visible(false);
        power_down_chain();
        Ok(())
    }

    async fn resume(&mut self) -> Result<(), Self::Error> {
        wait_for_chain_power().await;
        Ok(())
    }

    fn retry_after(
        &self,
        _operation: rmk::lighting::OutputOperation,
        _error: &Self::Error,
    ) -> Option<NonZeroU32> {
        NonZeroU32::new(50)
    }
}

/// Compiled-in effect defaults, used on a board that has never persisted a
/// selection. Keep these aligned with `config/glove80.toml` in the outer
/// configuration repository so its release artifacts and runtime profile boot
/// into the same tuned Crosshair/Amber setup.
const DEFAULT_EFFECT: u8 = Effect::<REACTIVE_HITS>::CROSSHAIR_INDEX;
const DEFAULT_EFFECT_VAL: u8 = 0xff;
const DEFAULT_EFFECT_SPEED: u8 = 108;
const DEFAULT_EFFECT_PARAMS: [u8; MAX_INITIAL_PARAMS] = [0, 90, 11, 170, 1, 0, 173, 0];

/// Effect index used when migrating a persisted selection of the retired
/// combined Storm effect into its Rain plus Reactive representation.
const LEGACY_STORM_OVERLAY: u8 = 6;

#[derive(Default)]
pub struct Preferences {
    extension: Option<LightingExtensionRecord>,
    overlay: Option<LightingExtensionOverlayRecord>,
    wake_layers: Option<u64>,
    output_mode: Option<rmk::types::protocol::rynk::LightingOutputMode>,
    params: rmk::heapless::Vec<LightingExtensionParamsRecord, { Effect::<1>::NAMES.len() }>,
}

pub async fn load_preferences<
    F: embedded_storage_async::nor_flash::NorFlash,
    const R: usize,
    const C: usize,
    const L: usize,
    const E: usize,
>(
    storage: &mut Storage<F, R, C, L, E>,
) -> Preferences {
    let mut preferences = Preferences {
        extension: storage.read_lighting_extension_state().await,
        overlay: storage.read_lighting_extension_overlay().await,
        wake_layers: storage.read_lighting_wake_layers().await,
        output_mode: storage.read_lighting_output_mode().await,
        params: rmk::heapless::Vec::new(),
    };
    const {
        assert!(MAX_INITIAL_PARAMS <= rmk::types::protocol::rynk::LIGHTING_EXTENSION_PARAM_CHUNK);
    }
    for effect in 0..Effect::<1>::NAMES.len() {
        if let Some(record) = storage
            .read_lighting_extension_params(effect as u8, 0)
            .await
        {
            let _ = preferences.params.push(record);
        }
    }
    preferences
}

pub fn engine(preferences: Preferences) -> Engine {
    let persisted_extension = preferences.extension;
    let persisted_overlay = preferences.overlay;
    let persisted_wake_layers = preferences.wake_layers;
    // Effect index 7 used to mean the combined Storm effect and now means
    // Crosshair. Old Storm advertised at most six parameters, while every
    // Crosshair record stores its seven-parameter row. Together with the
    // absence of a separately persisted overlay record, that distinguishes
    // old selections without sacrificing Crosshair's stable index.
    let legacy_storm = persisted_overlay.is_none()
        && persisted_extension.as_ref().is_some_and(|record| {
            record.effect == Effect::<REACTIVE_HITS>::LEGACY_STORM_INDEX && record.param_len <= 6
        });
    // A persisted selection wins over the compiled defaults, so changing what
    // the board boots into never means rebuilding firmware. Every index is
    // re-validated downstream against the live effect/palette/parameter lists,
    // because a record written before an effect was inserted would otherwise
    // name a different one.
    let mut config = PaletteFxConfig {
        initial_enabled: true,
        initial_val: DEFAULT_EFFECT_VAL,
        initial_speed: DEFAULT_EFFECT_SPEED,
        initial_palette: palette_id::AMBER,
        initial_effect: DEFAULT_EFFECT,
        initial_overlay: None,
        initial_params: DEFAULT_EFFECT_PARAMS,
        initial_param_len: CrosshairParams::COUNT,
        ..PaletteFxConfig::default()
    };
    if let Some(record) = persisted_extension {
        config.initial_effect = if legacy_storm {
            Effect::<REACTIVE_HITS>::RAIN_INDEX
        } else {
            record.effect
        };
        // Pre-layering records had no overlay key. Only the retired Storm
        // selection implies one; every other saved effect remains standalone.
        config.initial_overlay = legacy_storm.then_some(LEGACY_STORM_OVERLAY);
        config.initial_palette = record.palette as usize;
        config.initial_speed = record.speed;
        // A persisted zero means the user left the effects toggled off; keep
        // them off, and let RgbTog come back at the compiled brightness.
        config.initial_enabled = record.value != 0;
        config.initial_val = if record.value != 0 {
            record.value
        } else {
            DEFAULT_EFFECT_VAL
        };
        let restored = record.params();
        config.initial_param_len = restored.len() as u8;
        config.initial_params[..restored.len()].copy_from_slice(restored);
    }
    if let Some(record) = persisted_overlay {
        config.initial_overlay = record.effect;
        let restored = record.params();
        config.initial_overlay_param_len = restored.len() as u8;
        config.initial_overlay_params[..restored.len()].copy_from_slice(restored);
    }
    let mut palettefx = PaletteFxSource::new(
        TopologyLayout::new(&topology_config::LIGHTING_TOPOLOGY),
        &HIT_QUEUE,
        config,
    );
    lighting_preferences::restore_params(
        &mut palettefx,
        config.initial_effect,
        config.initial_overlay,
        DEFAULT_EFFECT,
        &DEFAULT_EFFECT_PARAMS[..CrosshairParams::COUNT as usize],
        &preferences.params,
    );
    let mut controls = crate::LIGHTING_CONTROLS;
    if let Some(wake_layers) = persisted_wake_layers {
        controls.wake_layers = wake_layers;
    }
    if let Some(mode) = preferences.output_mode {
        use rmk::types::protocol::rynk::LightingOutputMode;
        controls.initial_output_mode = match mode {
            LightingOutputMode::AlwaysOn => rmk::lighting::OutputMode::AlwaysOn,
            LightingOutputMode::AlwaysOff => rmk::lighting::OutputMode::AlwaysOff,
            LightingOutputMode::PoweredOnly => rmk::lighting::OutputMode::PoweredOnly,
        };
    }
    Engine::new(
        crate::LIGHTING_BACKGROUND,
        crate::LIGHTING_LAYER_SCENES,
        palettefx,
        ConditionalScenes::new(&crate::LIGHTING_CONDITIONAL_SCENE_CELLS, &BOARD_BATTERIES),
    )
    .with_controls(controls)
    .with_battery_status_provider(&BOARD_BATTERIES)
}

#[rmk::macros::processor(subscribe = [MaintenanceModeEvent])]
pub struct MaintenanceLightingState;

impl MaintenanceLightingState {
    async fn on_maintenance_mode_event(&mut self, _event: MaintenanceModeEvent) {
        CORE_MAILBOX.snapshot_changed();
    }
}

/// Feed pressed keys to the typing-reactive PaletteFx effects. Key
/// positions arrive in the local event bus's coordinates:
/// board-wide on the central (the split driver re-publishes peripheral keys
/// with their `[[split.peripheral]]` offsets applied), half-local on the
/// peripheral (its matrix scanner publishes unshifted scan positions).
/// Offsets shift them into the board-wide lighting matrix. The central records
/// both halves locally, while its own left-half hits are mirrored over the
/// split application channel. The peripheral records right-half scans locally
/// and mirrored left-half slots remotely, giving both engines the same spatial
/// hit while avoiding a duplicate for right-half keys.
///
/// Recording is render-neutral unless a key-reactive effect is active; the
/// source drains the queue either way and timestamps hits in the engine
/// animation-clock domain.
#[rmk::macros::processor(subscribe = [KeyboardEvent])]
pub struct ReactiveKeyHits {
    row_offset: u8,
    col_offset: u8,
    first_col: u8,
    last_col: u8,
    mirror_left_hits: bool,
}

impl ReactiveKeyHits {
    /// Central event bus: positions are already board-wide, including
    /// re-published peripheral events. Render every hit locally and mirror
    /// left-half hits to the peripheral.
    pub const fn central() -> Self {
        Self {
            row_offset: 0,
            col_offset: 0,
            first_col: 0,
            last_col: 14,
            mirror_left_hits: true,
        }
    }

    /// Peripheral event bus: shift local scans by the right half's
    /// `[[split.peripheral]]` offsets from keyboard.toml.
    pub const fn peripheral() -> Self {
        Self {
            row_offset: 0,
            col_offset: 7,
            first_col: 7,
            last_col: 14,
            mirror_left_hits: false,
        }
    }

    async fn on_keyboard_event(&mut self, event: KeyboardEvent) {
        if !event.pressed {
            return;
        }
        let KeyboardEventPos::Key(pos) = event.pos else {
            return;
        };
        let (Some(row), Some(col)) = (
            pos.row.checked_add(self.row_offset),
            pos.col.checked_add(self.col_offset),
        ) else {
            return;
        };
        if !(self.first_col..self.last_col).contains(&col) {
            return;
        }
        let key = MatrixPosition::new(row, col);
        let mut queued = false;
        for (slot, _) in topology_config::LIGHTING_TOPOLOGY.leds_for_key(key) {
            queued |= HIT_QUEUE.record(slot.0 as u8);
            if self.mirror_left_hits {
                crate::split_lighting::try_queue_effect_hit(slot);
            }
        }
        if queued {
            CORE_MAILBOX.snapshot_changed();
        }
    }
}

static PERIPHERAL_CONTEXT: BlockingMutex<rmk::RawMutex, Cell<LightingContext>> =
    BlockingMutex::new(Cell::new(LightingContext {
        layers: LayerState::new(0, 0, 1),
        indicators: IndicatorState {
            num_lock: false,
            caps_lock: false,
            scroll_lock: false,
            compose: false,
            kana: false,
        },
        powered: false,
        local_powered: false,
        // Replaced by the central's bitmap on the first replicated context;
        // until then the peripheral knows of no bonds.
        bonded_slots: 0,
        connection: rmk::types::connection::ConnectionStatus::new(),
        maintenance_unlocked: false,
        split_transport: rmk::lighting::SplitTransportState {
            auto: false,
            force: rmk::lighting::SplitForce::Auto,
            wired: false,
        },
    }));

#[derive(Clone, Copy)]
pub struct PeripheralState;

impl PeripheralState {
    fn set(context: LightingContext) {
        PERIPHERAL_CONTEXT.lock(|current| current.set(context));
    }

    fn apply_replicated(context: crate::split_lighting::ReplicatedContext) {
        PERIPHERAL_CONTEXT.lock(|current| {
            let mut merged = current.get();
            context.apply_to(&mut merged);
            current.set(merged);
        });
    }

    fn merge_ephemeral(context: &mut LightingContext) {
        context.layers = PERIPHERAL_CONTEXT.lock(Cell::get).layers;
        context.connection = rmk::state::current_connection_status();
    }

    /// Apply RMK's native high-priority effective-layer edge immediately.
    /// The local bitmap is conservatively adjusted without semantic sync.
    fn set_effective_layer(layer: u8) {
        PERIPHERAL_CONTEXT.lock(|current| {
            let mut context = current.get();
            let previous = context.layers;
            let mut active = previous.active_bits();
            if previous.effective != previous.default && previous.effective != layer {
                active &= !(1_u64 << previous.effective);
            }
            active |= (1_u64 << previous.default) | (1_u64 << layer);
            context.layers = LayerState::new(layer, previous.default, active);
            current.set(context);
        });
    }
}

/// Bridge RMK's native priority layer edge directly into the renderer.
#[rmk::macros::processor(subscribe = [LayerChangeEvent])]
pub struct FastPeripheralLayerLighting;

impl FastPeripheralLayerLighting {
    async fn on_layer_change_event(&mut self, event: LayerChangeEvent) {
        PeripheralState::set_effective_layer(event.0);
        CORE_MAILBOX.snapshot_changed();
    }
}

impl SnapshotProvider for PeripheralState {
    type Snapshot = LightingContext;

    fn snapshot(&self) -> Self::Snapshot {
        let mut context = PERIPHERAL_CONTEXT.lock(Cell::get);
        // `powered` stays the authority's VBUS, replicated over the split
        // link; this half's own VBUS goes in `local_powered`. The engine
        // picks between them per `powered_only_scope`.
        context.local_powered = local_vbus_present();
        // The split link replicates the central's connection status into this
        // half's own global; the lighting context packet does not carry it.
        context.connection = rmk::state::current_connection_status();
        context
    }
}

fn local_vbus_present() -> bool {
    embassy_nrf::pac::POWER.usbregstatus().read().vbusdetect()
}

/// Poll this half's local VBUS bit; on a change, invalidate static lighting
/// (under the local powered-only scope) and publish the charge state.
///
/// The board has no charger-status line configured, so VBUS presence stands
/// in for the charge state: plugged is reported as charging, unplugged as
/// discharging. There is no charge-termination detection, so a full battery
/// still reads as charging while wired. Without this proxy no charge state
/// is ever produced at all -- rmk's `ChargingStateReader` needs a
/// charger-detect GPIO this board does not wire up -- and every
/// `charge`-gated lighting rule is dead.
pub struct PowerMonitor {
    powered: bool,
    sleeping: bool,
    status_pwm: SimplePwm<'static>,
}

/// The status LED's duty in the polarity the board wires it: MoErgo's
/// boards drive theirs active-low, the Imprint's is active-high.
fn status_duty(duty: u16) -> DutyCycle {
    if crate::BOARD_STATUS_LED_ACTIVE_LOW {
        DutyCycle::inverted(duty)
    } else {
        DutyCycle::normal(duty)
    }
}

pub fn power_monitor(
    pwm: Peri<'static, PWM0>,
    status_led_pin: Peri<'static, impl Pin>,
) -> PowerMonitor {
    let powered = local_vbus_present();
    let mut pwm_config = SimpleConfig::default();
    pwm_config.prescaler = Prescaler::Div1;
    pwm_config.max_duty = STATUS_PWM_TOP;
    let mut status_pwm = SimplePwm::new_1ch(pwm, status_led_pin, &pwm_config);
    status_pwm.set_duty(0, status_duty(if powered { STATUS_PWM_DUTY } else { 0 }));
    PowerMonitor {
        powered,
        sleeping: false,
        status_pwm,
    }
}

impl PowerMonitor {
    fn update_chain_power(&self) {
        update_chain_power(self.powered, self.sleeping);
    }

    fn update_status_led(&mut self) {
        let duty = if self.powered && !self.sleeping {
            STATUS_PWM_DUTY
        } else {
            0
        };
        self.status_pwm.set_duty(0, status_duty(duty));
    }

    fn refresh_power(&mut self) {
        let powered = local_vbus_present();
        if powered == self.powered {
            return;
        }
        self.powered = powered;
        self.update_chain_power();
        self.update_status_led();
        rmk::event::publish_event(rmk::event::ChargingStateEvent { charging: powered });
        if matches!(
            crate::LIGHTING_CONTROLS.powered_only_scope,
            rmk::lighting::PoweredOnlyScope::Local
        ) {
            CORE_MAILBOX.snapshot_changed();
        }
    }
}

impl Runnable for PowerMonitor {
    async fn run(&mut self) -> ! {
        // Mirror rmk's ChargingStateReader settle: give the first battery ADC
        // reading a head start, then announce the boot-time state so the
        // charge state is defined without waiting for a plug/unplug edge.
        Timer::after_secs(2).await;
        self.powered = local_vbus_present();
        self.update_chain_power();
        rmk::event::publish_event(rmk::event::ChargingStateEvent {
            charging: self.powered,
        });
        self.update_status_led();
        let mut sleep = SleepStateEvent::subscriber();
        loop {
            let interval = if self.sleeping {
                SLEEP_POWER_POLL_INTERVAL
            } else {
                POWER_POLL_INTERVAL
            };
            match embassy_futures::select::select(sleep.next_event(), Timer::after(interval)).await
            {
                embassy_futures::select::Either::First(event) => {
                    self.sleeping = event.0;
                    self.refresh_power();
                    self.update_chain_power();
                    self.update_status_led();
                }
                embassy_futures::select::Either::Second(()) => self.refresh_power(),
            }
        }
    }
}

pub fn init_peripheral(
    spi: Peri<'static, SPI3>,
    data_pin: Peri<'static, impl Pin>,
    chain_power_pin: Peri<'static, impl Pin>,
) -> LightingProcessor<'static, PeripheralState, Engine, HalfOutput, COMMAND_CAPACITY> {
    // The peripheral never persists a selection: it renders whatever the
    // central replicates to it, so it boots on the compiled defaults.
    let service = LightingService::new(
        PeripheralState,
        engine(Preferences::default()),
        LogicalFrame::new(Rgb8::BLACK),
    )
    .with_present_interval(PRESENT_REFRESH_INTERVAL);
    let output = HalfOutput::right(LightingHardware::new(spi, data_pin, chain_power_pin));
    LightingProcessor::new(service, output, &CORE_MAILBOX)
}

fn try_send_diagnostic(message: crate::split_lighting::Message) -> bool {
    // Diagnostics and heartbeats only enter an empty queue, preserving the
    // remaining slots for replication Acks that can arrive while a packet
    // is pending.
    crate::split_lighting::diagnostic_may_enqueue(
        rmk::split_app::SPLIT_APP_PERIPH_TX.free_capacity(),
        rmk::split_app::SPLIT_APP_PERIPH_TX.capacity(),
    ) && rmk::split_app::SPLIT_APP_PERIPH_TX
        .try_send(message.encode())
        .is_ok()
}

/// Send the transport announcement out-of-line: inline closure bodies in
/// these giant joined futures have miscompiled into mid-instruction jumps
/// on thumbv7em (see the split-transport qualification notes), so arm
/// bodies stay in named functions. Returns whether a send is still owed.
/// Relay the persisted boot trace (and panic location, if any) once per
/// link-up. Out-of-line for the same reason as every other arm body here.
#[inline(never)]
fn relay_debug_now(step: u8) -> bool {
    if rmk::split_app::SPLIT_APP_LINK.try_get() != Some(true) {
        return false;
    }
    let sent = match step {
        0 => {
            let (stage, boots, rr, cause) = crate::debug_trace_parts();
            try_send_diagnostic(crate::split_lighting::Message::DebugTrace {
                stage,
                boots,
                rr,
                cause,
            })
        }
        _ => match crate::debug_panic_loc() {
            None => true,
            Some(loc) => {
                let mut text = [0u8; 23];
                let take = loc.len().min(23);
                text[..take].copy_from_slice(&loc.as_bytes()[..take]);
                try_send_diagnostic(crate::split_lighting::Message::DebugPanicLoc {
                    len: take as u8,
                    text,
                })
            }
        },
    };
    sent
}

#[inline(never)]
fn announce_transport_now() -> bool {
    let wired = rmk::split::selector::wired_selected();
    rmk::split_app::SPLIT_APP_LINK.try_get() == Some(true)
        && !try_send_diagnostic(crate::split_lighting::Message::TransportStatus {
            auto: true,
            wired,
        })
}

fn try_send_attestation(generation: u8) -> bool {
    LAST_DIGESTS.lock(Cell::get).is_some_and(|digests| {
        try_send_diagnostic(crate::split_lighting::Message::Attestation {
            generation,
            digests,
        })
    })
}

pub struct PeripheralLightingWorker;

pub const fn peripheral_lighting_worker() -> PeripheralLightingWorker {
    PeripheralLightingWorker
}

impl PeripheralLightingWorker {
    async fn apply(&mut self, request: ApplyRequest) {
        match CORE_MAILBOX
            .request(StandardCommand::ApplyReplica(&REPLICA_SLOT))
            .await
        {
            Ok(_) => {
                LAST_APPLIED_REVISION.lock(|revision| revision.set(Some(request.revision)));
                LAST_DIGESTS.lock(|digests| digests.set(Some(request.digests)));
                let ack = crate::split_lighting::Message::Ack {
                    generation: request.generation,
                    revision: request.revision,
                }
                .encode();
                if rmk::split_app::SPLIT_APP_PERIPH_TX.try_send(ack).is_err() {
                    defmt::warn!("lighting: peripheral replica ack queue full");
                    return;
                }
                // The Ack is now the head packet. With no other diagnostics
                // admitted into a non-empty queue, the remaining slot is safe
                // for its additive attestation.
                if let Some(digests) = LAST_DIGESTS.lock(Cell::get) {
                    let _ = rmk::split_app::SPLIT_APP_PERIPH_TX.try_send(
                        crate::split_lighting::Message::Attestation {
                            generation: request.generation,
                            digests,
                        }
                        .encode(),
                    );
                }
            }
            Err(_) => defmt::warn!("lighting: peripheral rejected replica"),
        }
    }

    async fn diagnostic(&mut self, request: DiagnosticRequest) {
        let message = match request {
            DiagnosticRequest::Status { request_id } => {
                match CORE_MAILBOX.request(StandardCommand::ReadState).await {
                    Ok(StandardReply::State(state)) => {
                        let layers = state.presented.map_or_else(
                            || PeripheralState.snapshot().layers,
                            |presented| presented.context.layers,
                        );
                        Some(crate::split_lighting::Message::StatusReport {
                            request_id,
                            applied_revision: LAST_APPLIED_REVISION.lock(Cell::get),
                            engine_revision: state.revision,
                            layers,
                            powered: state.powered,
                            wake_active: state.wake_active,
                            effective_output_enabled: state.output_enabled,
                        })
                    }
                    _ => None,
                }
            }
            DiagnosticRequest::FrameChunk { request_id, offset } => {
                match CORE_MAILBOX
                    .request(StandardCommand::ReadFrame { offset })
                    .await
                {
                    Ok(StandardReply::FramePage(page)) => {
                        let mut cells = [Rgb8::BLACK; 4];
                        let len = page.cells().len().min(cells.len());
                        cells[..len].copy_from_slice(&page.cells()[..len]);
                        Some(crate::split_lighting::Message::FrameChunk {
                            request_id,
                            revision: page.revision,
                            total: page.total,
                            start: page.start,
                            len: len as u8,
                            cells,
                        })
                    }
                    _ => None,
                }
            }
        };
        if let Some(message) = message {
            let _ = try_send_diagnostic(message);
        }
    }
}

impl Runnable for PeripheralLightingWorker {
    async fn run(&mut self) -> ! {
        loop {
            match embassy_futures::select::select(
                APPLY_REQUESTS.wait(),
                DIAGNOSTIC_REQUESTS.receive(),
            )
            .await
            {
                embassy_futures::select::Either::First(request) => self.apply(request).await,
                embassy_futures::select::Either::Second(request) => self.diagnostic(request).await,
            }
        }
    }
}

pub struct PeripheralReplication {
    stage: crate::split_lighting::SnapshotStage,
}

pub const fn peripheral_replication() -> PeripheralReplication {
    PeripheralReplication {
        stage: crate::split_lighting::SnapshotStage::new(),
    }
}

impl PeripheralReplication {
    async fn process(&mut self, data: rmk::split_app::SplitAppData) {
        crate::split_lighting::stage_note_pub(24);
        if data.payload() == [BOOTLOADER_TAG] {
            rmk::boot::jump_to_bootloader();
            return;
        }
        let Ok(message) = crate::split_lighting::Message::decode(data) else {
            crate::split_lighting::stage_note_pub(16);
            return;
        };
        if let crate::split_lighting::Message::EffectHit { slot } = message {
            if slot.index() < LEDS_PER_HALF && HIT_QUEUE.record(slot.0 as u8) {
                CORE_MAILBOX.snapshot_changed();
            }
            return;
        }
        match message {
            crate::split_lighting::Message::ReplicaProbe { generation } => {
                let _ = try_send_attestation(generation);
                return;
            }
            crate::split_lighting::Message::StatusRequest { request_id } => {
                let _ = DIAGNOSTIC_REQUESTS.try_send(DiagnosticRequest::Status { request_id });
                return;
            }
            crate::split_lighting::Message::FrameChunkRequest { request_id, offset } => {
                let _ = DIAGNOSTIC_REQUESTS
                    .try_send(DiagnosticRequest::FrameChunk { request_id, offset });
                return;
            }
            _ => {}
        }
        if let crate::split_lighting::Message::ContextUpdate {
            generation,
            revision,
            context,
            batteries,
        } = message
        {
            if LAST_APPLIED_REVISION.lock(Cell::get) == Some(revision) {
                set_battery_statuses(batteries);
                PeripheralState::apply_replicated(context);
                CORE_MAILBOX.snapshot_changed();
                let ack = crate::split_lighting::Message::Ack {
                    generation,
                    revision,
                }
                .encode();
                if rmk::split_app::SPLIT_APP_PERIPH_TX.try_send(ack).is_err() {
                    defmt::warn!("lighting: peripheral context ack queue full");
                }
            }
            return;
        }
        let Some((generation, mut snapshot, batteries)) = self.stage.apply(message) else {
            return;
        };
        PeripheralState::merge_ephemeral(&mut snapshot.context);
        let digests = crate::split_lighting::replica_digests(&snapshot);
        set_battery_statuses(batteries);
        PeripheralState::set(snapshot.context);
        let revision = snapshot.revision;
        if REPLICA_SLOT.put(snapshot).is_err() {
            defmt::warn!("lighting: peripheral replica slot busy");
            return;
        }
        APPLY_REQUESTS.signal(ApplyRequest {
            generation,
            revision,
            digests,
        });
    }
}

/// True on boards whose split transport is runtime-selected by cable detect.
/// Without an automatic policy both selector predicates hold at once, and a
/// transport announcement would carry no information.
fn auto_split_enabled() -> bool {
    !(rmk::split::selector::wired_selected() && rmk::split::selector::wireless_selected())
}

/// Resolves when the peripheral should (re)announce its transport: promptly
/// while an announcement is pending a free queue slot, otherwise on the next
/// selector edge. Boards without an automatic policy never announce.
async fn transport_announce_due(pending: bool, wired: bool) {
    if pending {
        Timer::after_millis(250).await;
        return;
    }
    if !auto_split_enabled() {
        core::future::pending::<()>().await;
    }
    if wired {
        rmk::split::selector::wait_wireless_selected().await;
    } else {
        rmk::split::selector::wait_wired_selected().await;
    }
}

impl Runnable for PeripheralReplication {
    async fn run(&mut self) -> ! {
        let mut link = rmk::split_app::SPLIT_APP_LINK
            .receiver()
            .expect("lighting replication owns one split-link receiver");
        let mut heartbeat_at = embassy_time::Instant::now() + ATTESTATION_INTERVAL;
        let mut announce_transport = auto_split_enabled();
        // Debug relay: 2 = trace owed, 1 = panic-loc owed, 0 = done for this
        // link session. Runs after the transport announcement drains.
        let mut relay_debug: u8 = 2;
        loop {
            let wired = rmk::split::selector::wired_selected();
            match embassy_futures::select::select4(
                link.changed(),
                rmk::split_app::SPLIT_APP_RX.receive(),
                Timer::at(heartbeat_at),
                transport_announce_due(announce_transport || relay_debug > 0, wired),
            )
            .await
            {
                embassy_futures::select::Either4::First(up) => {
                    self.stage.reset();
                    if up {
                        announce_transport = auto_split_enabled();
                        relay_debug = 2;
                    }
                    // Drain the inbox only when the link went down. This half
                    // marks the link up on the first *inbound* message, so on
                    // the up edge the inbox already holds the head of the
                    // reconnect snapshot -- draining here threw that snapshot
                    // away and cost every reconnect a wasted burst plus the
                    // central's ack timeout. Anything genuinely stale that
                    // survives an up edge is harmless: the next Begin restarts
                    // staging, and generation/revision matching rejects the
                    // rest.
                    if !up {
                        while rmk::split_app::SPLIT_APP_RX.try_receive().is_ok() {}
                    }
                }
                embassy_futures::select::Either4::Second(message) => self.process(message).await,
                embassy_futures::select::Either4::Third(()) => {
                    heartbeat_at += ATTESTATION_INTERVAL;
                    if rmk::split_app::SPLIT_APP_LINK.try_get() == Some(true) {
                        let _ = try_send_attestation(0);
                        // Periodic re-announcement heals a transport report
                        // lost to a saturated diagnostic queue.
                        announce_transport = auto_split_enabled();
                    }
                }
                embassy_futures::select::Either4::Fourth(()) => {
                    // One owed packet per firing: the diagnostic queue only
                    // admits into an empty queue, so sending two here would
                    // starve whichever went second.
                    let edge = rmk::split::selector::wired_selected() != wired;
                    if edge || announce_transport {
                        announce_transport = announce_transport_now();
                    } else if relay_debug > 0 && relay_debug_now(2 - relay_debug) {
                        relay_debug -= 1;
                    }
                }
            }
        }
    }
}
