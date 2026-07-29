//! `LightingSource` adapter for RMK's topology-aware lighting engine.
//!
//! Enabled by the `rmk-lighting` feature. Boards plug the PaletteFx effects
//! into a `StandardLightingEngine`'s extension band by instantiating
//! [`PaletteFxSource`] with their [`LedLayout`](crate::layout::LedLayout) and
//! LED count, e.g. `PaletteFxSource<VoyagerLayout, 52>` or
//! `PaletteFxSource<Glove80Layout, 80>`. The source claims the RGB
//! mode/palette/value/speed `LightAction`s ahead of the engine's uniform
//! background, so `light!(RgbModeForward)`-style keys drive the effects.
//!
//! Reactive key hits arrive through a board-owned [`HitQueue`] static: an
//! event task records LED indices and the source timestamps them in its own
//! animation-clock domain when it drains them on the next frame tick.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use heapless::Deque;
use rmk::lighting::compositor::{
    Contribution, ExtensionDescriptor, ExtensionParamSpec, ExtensionState, LightingSource,
    RenderInput,
};
use rmk::lighting::{EffectSample, LedSlot, Rgb8};
use rmk::types::action::LightAction;

use rmk::lighting::topology::LightingTopology;

use crate::color::{Hsv, hsv_to_rgb};
use crate::effects::{Effect, FlowState, FrameParams, RainParams};
use crate::layout::LedLayout;
use crate::palette::{BUILTIN_PALETTE_NAMES, BUILTIN_PALETTES};

/// [`LedLayout`] derived from an RMK lighting topology, so boards that
/// declare emitter geometry in `keyboard.toml` need no hand-written LED
/// position table. Each slot takes its effective position (explicit emitter
/// position, else the key center), normalised to the 0..=255 effect grid.
/// Both axes share one scale factor so polar effects stay isotropic.
pub struct TopologyLayout<const N: usize> {
    positions: [(u8, u8); N],
}

impl<const N: usize> TopologyLayout<N> {
    pub fn new(topology: &LightingTopology<'_>) -> Self {
        let mut raw = [(0i32, 0i32); N];
        let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
        let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
        for (slot, entry) in raw.iter_mut().enumerate() {
            let (x, y) = topology
                .effective_position(LedSlot(slot as u16))
                .map(|p| (p.x.raw() as i32, p.y.raw() as i32))
                .unwrap_or((0, 0));
            *entry = (x, y);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let span = (max_x - min_x).max(max_y - min_y).max(1);
        let mut positions = [(0u8, 0u8); N];
        for (slot, &(x, y)) in raw.iter().enumerate() {
            positions[slot] = (
                ((x - min_x) * 255 / span) as u8,
                ((y - min_y) * 255 / span) as u8,
            );
        }
        Self { positions }
    }
}

impl<const N: usize> LedLayout for TopologyLayout<N> {
    fn count(&self) -> usize {
        N
    }

    fn position(&self, index: usize) -> (u8, u8) {
        self.positions[index]
    }
}

/// Runtime tuning for a [`PaletteFxSource`]. `Default` matches PaletteFx's
/// QMK defaults; boards usually override the initial state for their hardware.
#[derive(Copy, Clone, Debug)]
pub struct PaletteFxConfig {
    /// Whether the effect source starts enabled. A disabled source remembers
    /// `initial_val` as the value restored by `RgbTog`.
    pub initial_enabled: bool,
    /// Initial brightness (`FrameParams::val`), adjusted by RgbVai/RgbVad.
    pub initial_val: u8,
    /// Brightness floor for RgbVad.
    pub min_val: u8,
    /// Brightness step per RgbVai/RgbVad press.
    pub val_step: u8,
    /// Initial animation speed, adjusted by RgbSpi/RgbSpd.
    pub initial_speed: u8,
    /// Speed step per RgbSpi/RgbSpd press.
    pub speed_step: u8,
    /// Index into [`BUILTIN_PALETTES`] to start on.
    pub initial_palette: usize,
    /// Index into [`Effect::NAMES`] to boot into. Out-of-range values fall
    /// back to Flow. A board that persists the live selection restores it
    /// here, so this is the boot selection whether it came from flash or
    /// from the board's compiled-in choice.
    pub initial_effect: u8,
    /// Parameter values for `initial_effect`, in advertised index order, as
    /// restored from persistent storage. Only the first `initial_param_len`
    /// entries are read.
    ///
    /// Each value is applied through the same bounds check a host set uses,
    /// so a record written by a firmware whose parameter list has since
    /// changed contributes only the values that are still valid instead of
    /// corrupting the effect.
    pub initial_params: [u8; MAX_INITIAL_PARAMS],
    /// Valid entries in `initial_params`; 0 means "use each default".
    pub initial_param_len: u8,
    /// Frame cadence in milliseconds (40 ≈ 25 fps).
    pub frame_interval_ms: u64,
}

impl Default for PaletteFxConfig {
    fn default() -> Self {
        Self {
            initial_enabled: true,
            initial_val: 255,
            min_val: 8,
            val_step: 16,
            initial_speed: 128,
            speed_step: 16,
            initial_palette: 0,
            // An animated default shows the system is alive.
            initial_effect: Effect::<1>::FLOW_INDEX,
            initial_params: [0; MAX_INITIAL_PARAMS],
            initial_param_len: 0,
            frame_interval_ms: 40,
        }
    }
}

/// Most parameters a board can restore at construction. Matches the chunk the
/// rynk protocol pages parameters in, which bounds any persisted record.
pub const MAX_INITIAL_PARAMS: usize = 8;

/// Number of effects in the cycle; sizes the per-effect parameter tables.
/// `Effect::NAMES` is the same list for every `HITS`, so the smallest
/// instantiation stands in for all of them in const position.
const EFFECT_COUNT: usize = Effect::<1>::NAMES.len();

/// The Rain tuning knobs as RMK parameter specs, derived from the effect's
/// own bounds so the advertised defaults cannot drift from
/// [`RainParams::DEFAULT`].
static RAIN_PARAM_SPECS: [ExtensionParamSpec; RainParams::COUNT as usize] = rain_param_specs();

const fn rain_param_specs() -> [ExtensionParamSpec; RainParams::COUNT as usize] {
    let mut specs = [ExtensionParamSpec {
        name: "",
        min: 0,
        max: 0,
        default: 0,
    }; RainParams::COUNT as usize];
    let mut i = 0;
    while i < specs.len() {
        specs[i] = ExtensionParamSpec {
            name: RainParams::NAMES[i],
            min: RainParams::MINS[i],
            max: RainParams::MAXES[i],
            default: RainParams::DEFAULTS[i],
        };
        i += 1;
    }
    specs
}

/// Per-effect parameter rows, indexed exactly like `Effect::NAMES`. Rain and
/// Storm share one row: Storm's background is the rain, so it takes the same
/// knobs. Every other effect advertises none.
static EFFECT_PARAMS: [&[ExtensionParamSpec]; EFFECT_COUNT] = [
    &[],               // Gradient
    &[],               // Flow
    &[],               // Vortex
    &[],               // Sparkle
    &[],               // Ripple
    &RAIN_PARAM_SPECS, // Rain
    &[],               // Reactive
    &RAIN_PARAM_SPECS, // Storm
];

/// Pending Reactive key-hit LED indices. `HITS` bounds how many un-drained
/// presses are remembered between frames.
pub struct HitQueue<const HITS: usize> {
    hits: BlockingMutex<CriticalSectionRawMutex, RefCell<Deque<u8, HITS>>>,
}

impl<const HITS: usize> HitQueue<HITS> {
    pub const fn new() -> Self {
        Self {
            hits: BlockingMutex::new(RefCell::new(Deque::new())),
        }
    }

    /// Record a key press. Returns `true` if the hit was queued (queue not
    /// full), so callers can skip requesting a render otherwise.
    pub fn record(&self, led_idx: u8) -> bool {
        self.hits
            .lock(|hits| hits.borrow_mut().push_back(led_idx).is_ok())
    }
}

impl<const HITS: usize> Default for HitQueue<HITS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Dense extension source running the PaletteFx effects over all `N` LEDs.
pub struct PaletteFxSource<L: LedLayout, const N: usize, const HITS: usize = 16> {
    layout: L,
    hits: &'static HitQueue<HITS>,
    config: PaletteFxConfig,
    effect: Effect<HITS>,
    palette_idx: usize,
    val: u8,
    last_nonzero_val: u8,
    speed: u8,
    /// Per-effect parameter values, indexed like `Effect::NAMES`. Held here
    /// rather than in the effect state so tuning survives cycling away from
    /// an effect and back; rows for effects without parameters are unused.
    rain_params: [RainParams; EFFECT_COUNT],
    frame: [Hsv; N],
    last_now_ms: u64,
}

impl<L: LedLayout, const N: usize, const HITS: usize> PaletteFxSource<L, N, HITS> {
    pub fn new(layout: L, hits: &'static HitQueue<HITS>, config: PaletteFxConfig) -> Self {
        let effect = Effect::from_index(config.initial_effect, 0)
            .unwrap_or_else(|| Effect::Flow(FlowState::new()));
        let last_nonzero_val = if config.initial_val == 0 {
            config.min_val.max(1)
        } else {
            config.initial_val
        };

        // Seed the restored tuning into the row for the effect it belongs to.
        // `RainParams::set` rejects each out-of-range value on its own, so a
        // stale record degrades parameter by parameter rather than wholesale.
        let mut rain_params = [RainParams::DEFAULT; EFFECT_COUNT];
        if Effect::<HITS>::uses_rain_params(config.initial_effect)
            && let Some(row) = rain_params.get_mut(config.initial_effect as usize)
        {
            let restored = config
                .initial_params
                .iter()
                .take(config.initial_param_len as usize);
            for (index, &value) in restored.enumerate() {
                row.set(index as u8, value);
            }
        }

        let mut source = Self {
            layout,
            hits,
            effect,
            palette_idx: config.initial_palette,
            val: if config.initial_enabled {
                config.initial_val
            } else {
                0
            },
            last_nonzero_val,
            speed: config.initial_speed,
            config,
            rain_params,
            frame: [Hsv::default(); N],
            last_now_ms: 0,
        };
        source.sync_effect_params();
        source
    }

    /// Push the stored tuning of the active effect into its freshly built
    /// state. Called after every effect switch, since `Effect::from_index`
    /// and the cycle keys both hand back default-tuned state.
    fn sync_effect_params(&mut self) {
        let index = self.effect.index() as usize;
        if let Some(&params) = self.rain_params.get(index) {
            self.effect.set_rain_params(params);
        }
    }

    fn tick_frame(&mut self, now_ms: u64) {
        self.last_now_ms = now_ms;
        self.hits.hits.lock(|hits| {
            let mut hits = hits.borrow_mut();
            while let Some(led) = hits.pop_front() {
                self.effect
                    .record_hit(&self.layout, led as usize, now_ms as u32);
            }
        });
        self.effect.tick(
            &self.layout,
            FrameParams {
                palette: BUILTIN_PALETTES[self.palette_idx],
                speed: self.speed,
                sat: 255,
                val: self.val,
                timer_ms: now_ms as u32,
            },
            &mut self.frame,
        );
    }

    fn ripple_seed(&self) -> u64 {
        0xDEAD_C0DE_CAFE_F00D ^ self.last_now_ms
    }
}

impl<L: LedLayout, const N: usize, const HITS: usize, Context> LightingSource<Rgb8, Context>
    for PaletteFxSource<L, N, HITS>
{
    fn len(&self, _: &RenderInput<'_, Context>) -> usize {
        N
    }

    fn slot(&self, index: usize, _: &RenderInput<'_, Context>) -> LedSlot {
        LedSlot(index as u16)
    }

    fn contribution(
        &mut self,
        index: usize,
        input: &RenderInput<'_, Context>,
    ) -> Contribution<Rgb8> {
        if index == 0 {
            self.tick_frame(input.now_ms);
        }
        let rgb = hsv_to_rgb(self.frame[index]);
        Contribution::Opaque(EffectSample {
            color: Rgb8::new(rgb.r, rgb.g, rgb.b),
            next_change_ms: Some(input.now_ms + self.config.frame_interval_ms),
        })
    }

    fn handle_light_action(&mut self, action: LightAction) -> bool {
        let n = BUILTIN_PALETTES.len();
        match action {
            LightAction::RgbTog => {
                if self.val == 0 {
                    self.val = self.last_nonzero_val;
                } else {
                    self.last_nonzero_val = self.val;
                    self.val = 0;
                }
            }
            LightAction::RgbModeForward => {
                self.effect.next(self.ripple_seed());
                self.sync_effect_params();
            }
            LightAction::RgbModeReverse => {
                self.effect.prev(self.ripple_seed());
                self.sync_effect_params();
            }
            LightAction::RgbHui => self.palette_idx = (self.palette_idx + 1) % n,
            LightAction::RgbHud => self.palette_idx = (self.palette_idx + n - 1) % n,
            LightAction::RgbVai => {
                self.val = self.val.saturating_add(self.config.val_step);
                self.last_nonzero_val = self.val.max(self.config.min_val.max(1));
            }
            LightAction::RgbVad => {
                self.val = self
                    .val
                    .saturating_sub(self.config.val_step)
                    .max(self.config.min_val);
                if self.val != 0 {
                    self.last_nonzero_val = self.val;
                }
            }
            LightAction::RgbSpi => self.speed = self.speed.saturating_add(self.config.speed_step),
            LightAction::RgbSpd => self.speed = self.speed.saturating_sub(self.config.speed_step),
            _ => return false,
        }
        true
    }

    fn extension_descriptor(&self) -> Option<ExtensionDescriptor> {
        Some(ExtensionDescriptor {
            effects: &Effect::<HITS>::NAMES,
            palettes: &BUILTIN_PALETTE_NAMES,
            params: &EFFECT_PARAMS,
        })
    }

    fn extension_state(&self) -> Option<ExtensionState> {
        Some(ExtensionState {
            effect: self.effect.index(),
            palette: self.palette_idx as u8,
            value: self.val,
            speed: self.speed,
        })
    }

    fn apply_extension_state(&mut self, state: ExtensionState) -> bool {
        if (state.palette as usize) >= BUILTIN_PALETTES.len() {
            return false;
        }
        // Switching to the already-active effect keeps its animation state;
        // hosts re-sending the current selection must not reset the phase.
        if state.effect != self.effect.index() {
            let Some(effect) = Effect::from_index(state.effect, self.ripple_seed()) else {
                return false;
            };
            self.effect = effect;
            self.sync_effect_params();
        }
        self.palette_idx = state.palette as usize;
        self.val = state.value;
        if state.value != 0 {
            self.last_nonzero_val = state.value;
        }
        self.speed = state.speed;
        true
    }

    fn extension_param(&self, effect: u8, index: u8) -> Option<u8> {
        if !Effect::<HITS>::uses_rain_params(effect) {
            return None;
        }
        self.rain_params.get(effect as usize)?.get(index)
    }

    fn apply_extension_param(&mut self, effect: u8, index: u8, value: u8) -> bool {
        if !Effect::<HITS>::uses_rain_params(effect) {
            return false;
        }
        let Some(params) = self.rain_params.get_mut(effect as usize) else {
            return false;
        };
        // `set` is the range authority: it declines out-of-range values
        // without mutating, so a rejected set leaves the source untouched.
        if !params.set(index, value) {
            return false;
        }
        let params = *params;
        if effect == self.effect.index() {
            self.effect.set_rain_params(params);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;

    static HITS: HitQueue<1> = HitQueue::new();
    static NO_HITS: HitQueue<1> = HitQueue::new();
    static POSITIONS: [(u8, u8); 1] = [(128, 128)];

    fn reactive(hits: &'static HitQueue<1>) -> PaletteFxSource<SliceLayout<'static>, 1, 1> {
        let mut source = PaletteFxSource::new(
            SliceLayout::new(&POSITIONS),
            hits,
            PaletteFxConfig {
                initial_val: 255,
                ..PaletteFxConfig::default()
            },
        );
        source.effect = Effect::from_index(6, 0).unwrap();
        source
    }

    #[test]
    fn queued_hits_are_timestamped_in_the_render_clock_domain() {
        let mut baseline = reactive(&NO_HITS);
        baseline.tick_frame(60_000);

        let mut hit = reactive(&HITS);
        assert!(HITS.record(0));
        hit.tick_frame(60_000);

        assert_ne!(hit.frame, baseline.frame);
    }

    #[test]
    fn rgb_toggle_turns_effects_off_and_restores_the_previous_value() {
        let mut source = reactive(&NO_HITS);

        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::handle_light_action(
                &mut source,
                LightAction::RgbTog,
            )
        );
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_state(&source)
                .unwrap()
                .value,
            0
        );

        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::handle_light_action(
                &mut source,
                LightAction::RgbTog,
            )
        );
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_state(&source)
                .unwrap()
                .value,
            255
        );
    }

    /// Every effect that consumes rain tuning must advertise the rain row,
    /// and no other effect may: otherwise a host offers knobs that do
    /// nothing, or hides ones that work.
    #[test]
    fn advertised_parameter_rows_match_the_effects_that_use_them() {
        let source = reactive(&NO_HITS);
        let descriptor =
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_descriptor(&source)
                .unwrap();
        assert_eq!(descriptor.params.len(), descriptor.effects.len());
        for effect in 0..descriptor.effects.len() as u8 {
            let specs = descriptor.effect_params(effect).unwrap();
            assert_eq!(
                !specs.is_empty(),
                Effect::<1>::uses_rain_params(effect),
                "effect {effect} parameter row disagrees with its state"
            );
        }
        assert!(
            descriptor
                .effect_params(descriptor.effects.len() as u8)
                .is_none()
        );
    }

    /// The descriptor's advertised defaults are the tuned values, and a
    /// fresh source reads those values back.
    #[test]
    fn rain_parameters_default_to_the_tuned_values() {
        let source = reactive(&NO_HITS);
        let descriptor =
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_descriptor(&source)
                .unwrap();
        for effect in [Effect::<1>::RAIN_INDEX, Effect::<1>::STORM_INDEX] {
            let specs = descriptor.effect_params(effect).unwrap();
            assert_eq!(specs.len(), RainParams::COUNT as usize);
            for (index, spec) in specs.iter().enumerate() {
                assert_eq!(spec.name, RainParams::NAMES[index]);
                assert_eq!(spec.min, RainParams::MINS[index]);
                assert_eq!(spec.max, RainParams::MAXES[index]);
                assert_eq!(spec.default, RainParams::DEFAULTS[index]);
                assert_eq!(
                    <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_param(
                        &source,
                        effect,
                        index as u8,
                    ),
                    Some(spec.default),
                );
            }
        }
    }

    #[test]
    fn parameter_sets_round_trip_and_survive_effect_switching() {
        let mut source = reactive(&NO_HITS);
        let rain = Effect::<1>::RAIN_INDEX;

        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::apply_extension_param(
                &mut source,
                rain,
                2,
                200,
            )
        );
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_param(
                &source, rain, 2
            ),
            Some(200),
        );
        // Rain and Storm keep separate values even though they share specs.
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_param(
                &source,
                Effect::<1>::STORM_INDEX,
                2,
            ),
            Some(RainParams::DEFAULT.trail_len),
        );

        // Cycle all the way around: the freshly built Rain state must be
        // retuned from the source's stored values, not left at defaults.
        for _ in 0..Effect::<1>::NAMES.len() {
            assert!(
                <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::handle_light_action(
                    &mut source,
                    LightAction::RgbModeForward,
                )
            );
        }
        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::apply_extension_state(
                &mut source,
                ExtensionState {
                    effect: rain,
                    palette: 0,
                    value: 255,
                    speed: 128,
                },
            )
        );
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_param(
                &source, rain, 2
            ),
            Some(200),
        );
        assert_eq!(
            source.effect.rain_params(),
            Some(source.rain_params[rain as usize])
        );
    }

    #[test]
    fn out_of_range_and_unknown_parameters_are_declined_unchanged() {
        let mut source = reactive(&NO_HITS);
        let rain = Effect::<1>::RAIN_INDEX;
        let before = source.rain_params;

        // Out of range for its spec, an unknown index, and an effect with no
        // parameters at all.
        for (effect, index, value) in [
            (rain, 0, RainParams::DROPS_MAX + 1),
            (rain, RainParams::COUNT, 1),
            (0, 0, 1),
            (Effect::<1>::NAMES.len() as u8, 0, 1),
        ] {
            assert!(
                !<PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::apply_extension_param(
                    &mut source,
                    effect,
                    index,
                    value,
                ),
                "effect {effect} index {index} value {value} should be declined"
            );
        }
        assert_eq!(source.rain_params, before);
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_param(&source, 0, 0),
            None,
        );
    }

    /// A parameter set on the active effect reaches the live effect state,
    /// not just the source's table.
    #[test]
    fn parameter_sets_change_what_the_active_effect_renders() {
        static WIDE: [(u8, u8); 1] = [(0, 128)];
        let build = || {
            let mut source: PaletteFxSource<SliceLayout<'static>, 1, 1> = PaletteFxSource::new(
                SliceLayout::new(&WIDE),
                &NO_HITS,
                PaletteFxConfig::default(),
            );
            source.effect = Effect::from_index(Effect::<1>::RAIN_INDEX, 0).unwrap();
            source
        };

        let mut narrow = build();
        let mut wide = build();
        // A drop lands at x=128; only the wider column reaches the LED at
        // x=0. (Both sources share the seed, so they place the same drops.)
        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::apply_extension_param(
                &mut wide,
                Effect::<1>::RAIN_INDEX,
                3,
                RainParams::COLUMN_HALF_WIDTH_MAX,
            )
        );

        let mut differed = false;
        for t in (0..8000).step_by(40) {
            narrow.tick_frame(t);
            wide.tick_frame(t);
            differed |= narrow.frame != wide.frame;
        }
        assert!(differed, "column width never changed the rendered frame");
    }

    #[test]
    fn disabled_initial_state_toggles_on_at_the_configured_value() {
        let mut source = PaletteFxSource::new(
            SliceLayout::new(&POSITIONS),
            &NO_HITS,
            PaletteFxConfig {
                initial_enabled: false,
                initial_val: 128,
                ..PaletteFxConfig::default()
            },
        );

        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_state(&source)
                .unwrap()
                .value,
            0
        );
        assert!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::handle_light_action(
                &mut source,
                LightAction::RgbTog,
            )
        );
        assert_eq!(
            <PaletteFxSource<_, 1, 1> as LightingSource<Rgb8, ()>>::extension_state(&source)
                .unwrap()
                .value,
            128
        );
    }

    /// A board that persists the selection restores its parameters through the
    /// config, and a record from a firmware with different bounds must not be
    /// able to push a value the effect would reject from a host.
    #[test]
    fn restored_parameters_are_applied_and_individually_bounds_checked() {
        let boot = |initial_param_len, params: [u8; MAX_INITIAL_PARAMS]| {
            let source: PaletteFxSource<_, 1, 1> = PaletteFxSource::new(
                SliceLayout::new(&POSITIONS),
                &NO_HITS,
                PaletteFxConfig {
                    initial_effect: Effect::<1>::RAIN_INDEX,
                    initial_params: params,
                    initial_param_len,
                    ..PaletteFxConfig::default()
                },
            );
            source
                .effect
                .rain_params()
                .expect("Rain carries rain params")
        };

        let mut wanted = RainParams::DEFAULT;
        assert!(wanted.set(0, 3));
        assert!(wanted.set(2, 200));
        assert_eq!(boot(4, [3, 30, 200, 14, 0, 0, 0, 0]), wanted);

        // Zero is below every parameter's minimum: a truncated or empty record
        // must leave the defaults alone rather than zeroing the effect.
        assert_eq!(boot(0, [0; MAX_INITIAL_PARAMS]), RainParams::DEFAULT);
        assert_eq!(boot(4, [0; MAX_INITIAL_PARAMS]), RainParams::DEFAULT);

        // Only the out-of-range entry is dropped; the valid one still lands.
        let mut partial = RainParams::DEFAULT;
        assert!(partial.set(0, 2));
        assert_eq!(boot(2, [2, 0, 0, 0, 0, 0, 0, 0]), partial);
    }

    /// Nothing persists the live selection, so `initial_effect` is what the
    /// board shows on every boot. An out-of-range index must still leave an
    /// animated effect running rather than refusing to construct.
    #[test]
    fn the_configured_initial_effect_is_what_boots() {
        let boot = |initial_effect| {
            let source: PaletteFxSource<_, 1, 1> = PaletteFxSource::new(
                SliceLayout::new(&POSITIONS),
                &NO_HITS,
                PaletteFxConfig {
                    initial_effect,
                    ..PaletteFxConfig::default()
                },
            );
            source.effect.index()
        };

        assert_eq!(boot(Effect::<1>::STORM_INDEX), Effect::<1>::STORM_INDEX);
        assert_eq!(boot(Effect::<1>::RAIN_INDEX), Effect::<1>::RAIN_INDEX);
        assert_eq!(
            boot(Effect::<1>::NAMES.len() as u8),
            Effect::<1>::FLOW_INDEX
        );
    }
}
