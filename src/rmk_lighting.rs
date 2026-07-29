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
    Contribution, ExtensionDescriptor, ExtensionState, LightingSource, RenderInput,
};
use rmk::lighting::{EffectSample, LedSlot, Rgb8};
use rmk::types::action::LightAction;

use rmk::lighting::topology::LightingTopology;

use crate::color::{Hsv, hsv_to_rgb};
use crate::effects::{Effect, FrameParams};
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
            frame_interval_ms: 40,
        }
    }
}

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
    frame: [Hsv; N],
    last_now_ms: u64,
}

impl<L: LedLayout, const N: usize, const HITS: usize> PaletteFxSource<L, N, HITS> {
    pub fn new(layout: L, hits: &'static HitQueue<HITS>, config: PaletteFxConfig) -> Self {
        let mut effect = Effect::Gradient;
        // Boot into Flow: an animated default shows the system is alive.
        effect.next(0);
        let last_nonzero_val = if config.initial_val == 0 {
            config.min_val.max(1)
        } else {
            config.initial_val
        };
        Self {
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
            frame: [Hsv::default(); N],
            last_now_ms: 0,
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
            LightAction::RgbModeForward => self.effect.next(self.ripple_seed()),
            LightAction::RgbModeReverse => self.effect.prev(self.ripple_seed()),
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
        }
        self.palette_idx = state.palette as usize;
        self.val = state.value;
        if state.value != 0 {
            self.last_nonzero_val = state.value;
        }
        self.speed = state.speed;
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
}
