//! Runtime-switchable effect selection.
//!
//! Firmware that cycles through the PaletteFx effects at runtime (RGB
//! mode-forward/backward keys) needs one value that owns whichever effect
//! is active plus its state. [`Effect`] is that value: cycling resets the
//! outgoing effect's state so each effect starts from its own time phase.

use super::{
    CrosshairParams, CrosshairState, FlowState, FrameParams, KeyfallParams, KeyfallState, Pcg32,
    RainParams, RainState, ReactiveState, RippleState, ShockwaveState, SparkleState, TracerState,
    VortexState,
};
use crate::color::{Hsv, Rgb, blend_rgb, hsv_to_rgb};
use crate::layout::LedLayout;

/// Stream selector for the Ripple RNG; any odd constant works, this one is
/// from the PCG reference implementation.
const RIPPLE_RNG_STREAM: u64 = 0xA02B_DBF7_BB3C_0A7F;

/// Stream selector for the Rain RNG; distinct from Ripple's so the two
/// effects don't share drop-placement sequences when seeded alike.
const RAIN_RNG_STREAM: u64 = 0x5851_F42D_4C95_7F2D;

/// The currently active effect and its state. `HITS` sizes every key-reactive
/// effect's ring of remembered key presses.
pub enum Effect<const HITS: usize> {
    Gradient,
    Flow(FlowState),
    Vortex(VortexState),
    Sparkle(SparkleState),
    Ripple(RippleState, Pcg32),
    Rain(RainState, Pcg32),
    Reactive(ReactiveState<HITS>),
    Crosshair(CrosshairState<HITS>),
    Tracer(TracerState<HITS>),
    Keyfall(KeyfallState<HITS>),
    Shockwave(ShockwaveState<HITS>),
}

impl<const HITS: usize> Effect<HITS> {
    /// Display names in stable index order; `index`/`from_index` and the
    /// next/prev cycle all agree with this ordering.
    pub const NAMES: [&'static str; 11] = [
        "Gradient",
        "Flow",
        "Vortex",
        "Sparkle",
        "Ripple",
        "Rain",
        "Reactive",
        "Crosshair",
        "Tracer",
        "Keyfall",
        "Shockwave",
    ];

    /// Stable index of the Flow effect into [`Self::NAMES`].
    pub const FLOW_INDEX: u8 = 1;
    /// Stable index of the Rain effect into [`Self::NAMES`].
    pub const RAIN_INDEX: u8 = 5;
    /// The retired Storm index. It is now occupied by Crosshair, so callers
    /// restoring pre-layering records must migrate Storm before construction.
    pub const LEGACY_STORM_INDEX: u8 = 7;
    /// Stable index of the Crosshair effect into [`Self::NAMES`].
    pub const CROSSHAIR_INDEX: u8 = 7;
    /// Stable index of the Tracer effect into [`Self::NAMES`].
    pub const TRACER_INDEX: u8 = 8;
    /// Stable index of the Keyfall effect into [`Self::NAMES`].
    pub const KEYFALL_INDEX: u8 = 9;
    /// Stable index of the Shockwave effect into [`Self::NAMES`].
    pub const SHOCKWAVE_INDEX: u8 = 10;

    /// Whether the effect at `index` renders a [`RainState`], and therefore
    /// exposes the [`RainParams`] tuning.
    pub const fn uses_rain_params(index: u8) -> bool {
        index == Self::RAIN_INDEX
    }

    /// Whether the effect at `index` exposes [`CrosshairParams`].
    pub const fn uses_crosshair_params(index: u8) -> bool {
        index == Self::CROSSHAIR_INDEX
    }

    /// Whether the effect at `index` exposes [`KeyfallParams`].
    pub const fn uses_keyfall_params(index: u8) -> bool {
        index == Self::KEYFALL_INDEX
    }

    /// Retune the rain of whichever variant owns a [`RainState`]; a no-op
    /// for every other effect.
    pub const fn set_rain_params(&mut self, params: RainParams) {
        match self {
            Self::Rain(rain, _) => rain.set_params(params),
            _ => {}
        }
    }

    /// The rain tuning of whichever variant owns a [`RainState`], or `None`
    /// for effects that have none.
    pub const fn rain_params(&self) -> Option<RainParams> {
        match self {
            Self::Rain(rain, _) => Some(rain.params()),
            _ => None,
        }
    }

    /// Retune Crosshair; a no-op for every other effect.
    pub const fn set_crosshair_params(&mut self, params: CrosshairParams) {
        if let Self::Crosshair(state) = self {
            state.set_params(params);
        }
    }

    /// Crosshair tuning, or `None` for every other effect.
    pub const fn crosshair_params(&self) -> Option<CrosshairParams> {
        match self {
            Self::Crosshair(state) => Some(state.params()),
            _ => None,
        }
    }

    /// Retune Keyfall; a no-op for every other effect.
    pub const fn set_keyfall_params(&mut self, params: KeyfallParams) {
        if let Self::Keyfall(state) = self {
            state.set_params(params);
        }
    }

    /// Keyfall tuning, or `None` for every other effect.
    pub const fn keyfall_params(&self) -> Option<KeyfallParams> {
        match self {
            Self::Keyfall(state) => Some(state.params()),
            _ => None,
        }
    }

    /// Stable index of the active effect into [`Self::NAMES`].
    pub const fn index(&self) -> u8 {
        match self {
            Self::Gradient => 0,
            Self::Flow(_) => 1,
            Self::Vortex(_) => 2,
            Self::Sparkle(_) => 3,
            Self::Ripple(_, _) => 4,
            Self::Rain(_, _) => 5,
            Self::Reactive(_) => 6,
            Self::Crosshair(_) => 7,
            Self::Tracer(_) => 8,
            Self::Keyfall(_) => 9,
            Self::Shockwave(_) => 10,
        }
    }

    /// Fresh effect state for a stable index, or `None` when out of range.
    /// `ripple_seed` seeds Ripple's drop-placement RNG (see [`Self::next`]).
    pub fn from_index(index: u8, ripple_seed: u64) -> Option<Self> {
        Some(match index {
            0 => Self::Gradient,
            1 => Self::Flow(FlowState::new()),
            2 => Self::Vortex(VortexState::new()),
            3 => Self::Sparkle(SparkleState::new()),
            4 => Self::Ripple(RippleState::new(), ripple_rng(ripple_seed)),
            5 => Self::Rain(RainState::new(), rain_rng(ripple_seed)),
            6 => Self::Reactive(ReactiveState::new()),
            7 => Self::Crosshair(CrosshairState::new()),
            8 => Self::Tracer(TracerState::new()),
            9 => Self::Keyfall(KeyfallState::new()),
            10 => Self::Shockwave(ShockwaveState::new()),
            _ => return None,
        })
    }

    /// Render one frame of the active effect into `out`.
    pub fn tick<L: LedLayout>(&mut self, layout: &L, params: FrameParams<'_>, out: &mut [Hsv]) {
        match self {
            Self::Gradient => super::gradient(layout, params, out),
            Self::Flow(s) => s.tick(layout, params, out),
            Self::Vortex(s) => s.tick(layout, params, out),
            Self::Sparkle(s) => s.tick(layout, params, out),
            Self::Ripple(s, rng) => s.tick_with_rng(rng, layout, params, out),
            Self::Rain(s, rng) => s.tick_with_rng(rng, layout, params, out),
            Self::Reactive(s) => s.tick(layout, params, out),
            Self::Crosshair(s) => s.tick(layout, params, out),
            Self::Tracer(s) => s.tick(layout, params, out),
            Self::Keyfall(s) => s.tick(layout, params, out),
            Self::Shockwave(s) => s.tick(layout, params, out),
        }
    }

    /// Render the effect as a layer whose output value represents coverage.
    /// Most effects already spend value that way. Reactive's standalone dim
    /// wash is deliberately removed so only key hits cover the lower slot.
    pub fn tick_layer<L: LedLayout>(
        &mut self,
        layout: &L,
        params: FrameParams<'_>,
        out: &mut [Hsv],
    ) {
        match self {
            Self::Reactive(state) => state.tick_layer(layout, params, out),
            Self::Crosshair(state) => state.tick_layer(layout, params, out),
            Self::Tracer(state) => state.tick_layer(layout, params, out),
            Self::Keyfall(state) => state.tick_layer(layout, params, out),
            Self::Shockwave(state) => state.tick_layer(layout, params, out),
            _ => self.tick(layout, params, out),
        }
    }

    /// Switch to the next effect in the cycle. `ripple_seed` seeds the
    /// Ripple drop-placement RNG so its first drop lands on a different
    /// key each time Ripple is cycled to; pass the current time.
    pub fn next(&mut self, ripple_seed: u64) {
        *self = match self {
            Self::Gradient => Self::Flow(FlowState::new()),
            Self::Flow(_) => Self::Vortex(VortexState::new()),
            Self::Vortex(_) => Self::Sparkle(SparkleState::new()),
            Self::Sparkle(_) => Self::Ripple(RippleState::new(), ripple_rng(ripple_seed)),
            Self::Ripple(_, _) => Self::Rain(RainState::new(), rain_rng(ripple_seed)),
            Self::Rain(_, _) => Self::Reactive(ReactiveState::new()),
            Self::Reactive(_) => Self::Crosshair(CrosshairState::new()),
            Self::Crosshair(_) => Self::Tracer(TracerState::new()),
            Self::Tracer(_) => Self::Keyfall(KeyfallState::new()),
            Self::Keyfall(_) => Self::Shockwave(ShockwaveState::new()),
            Self::Shockwave(_) => Self::Gradient,
        };
    }

    /// Switch to the previous effect in the cycle; see [`Effect::next`].
    pub fn prev(&mut self, ripple_seed: u64) {
        *self = match self {
            Self::Gradient => Self::Shockwave(ShockwaveState::new()),
            Self::Shockwave(_) => Self::Keyfall(KeyfallState::new()),
            Self::Keyfall(_) => Self::Tracer(TracerState::new()),
            Self::Tracer(_) => Self::Crosshair(CrosshairState::new()),
            Self::Crosshair(_) => Self::Reactive(ReactiveState::new()),
            Self::Reactive(_) => Self::Rain(RainState::new(), rain_rng(ripple_seed)),
            Self::Rain(_, _) => Self::Ripple(RippleState::new(), ripple_rng(ripple_seed)),
            Self::Ripple(_, _) => Self::Sparkle(SparkleState::new()),
            Self::Sparkle(_) => Self::Vortex(VortexState::new()),
            Self::Vortex(_) => Self::Flow(FlowState::new()),
            Self::Flow(_) => Self::Gradient,
        };
    }

    /// Record a key press at `led_idx` against a key-reactive effect; no-op
    /// for every other variant. Returns `true` iff a hit was recorded, so the
    /// caller can skip a re-render that would not be visible.
    pub fn record_hit<L: LedLayout>(&mut self, layout: &L, led_idx: usize, timer_ms: u32) -> bool {
        let (x, y) = layout.position(led_idx);
        match self {
            Self::Reactive(state) => state.record_hit(x, y, timer_ms),
            Self::Crosshair(state) => state.record_hit(led_idx, x, y, timer_ms),
            Self::Tracer(state) => state.record_hit(x, y, timer_ms),
            Self::Keyfall(state) => state.record_hit(layout, x, y, timer_ms),
            Self::Shockwave(state) => state.record_hit(layout, x, y, timer_ms),
            _ => return false,
        }
        true
    }
}

/// A two-slot stack whose slots both accept the same effect set.
///
/// The primary is always present and is what mode-forward/reverse cycles.
/// The overlay is independently selectable and may contain any effect,
/// including another instance of the primary effect.
pub struct EffectStack<const HITS: usize> {
    primary: Effect<HITS>,
    overlay: Option<Effect<HITS>>,
}

impl<const HITS: usize> EffectStack<HITS> {
    pub const fn new(primary: Effect<HITS>, overlay: Option<Effect<HITS>>) -> Self {
        Self { primary, overlay }
    }

    pub const fn primary(&self) -> &Effect<HITS> {
        &self.primary
    }

    pub const fn primary_mut(&mut self) -> &mut Effect<HITS> {
        &mut self.primary
    }

    pub const fn overlay(&self) -> Option<&Effect<HITS>> {
        self.overlay.as_ref()
    }

    pub const fn overlay_mut(&mut self) -> Option<&mut Effect<HITS>> {
        self.overlay.as_mut()
    }

    pub fn set_primary(&mut self, effect: Effect<HITS>) {
        self.primary = effect;
    }

    pub fn set_overlay(&mut self, effect: Option<Effect<HITS>>) {
        self.overlay = effect;
    }

    /// Render both slots. The overlay's rendered value is its coverage; its
    /// hue/saturation are composited at the stack's global brightness. This
    /// keeps sparse effects sparse without applying brightness twice.
    pub fn tick<L: LedLayout>(
        &mut self,
        layout: &L,
        params: FrameParams<'_>,
        scratch: &mut [Hsv],
        out: &mut [Rgb],
    ) {
        self.primary.tick(layout, params, scratch);
        for (target, &pixel) in out.iter_mut().zip(scratch.iter()) {
            *target = hsv_to_rgb(pixel);
        }

        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let overlay_params = FrameParams { val: 255, ..params };
        overlay.tick_layer(layout, overlay_params, scratch);
        for (target, &pixel) in out.iter_mut().zip(scratch.iter()) {
            let coverage = pixel.v;
            let color = hsv_to_rgb(Hsv {
                v: params.val,
                ..pixel
            });
            *target = blend_rgb(*target, color, coverage);
        }
    }

    /// Record a press in every key-reactive instance in the stack.
    pub fn record_hit<L: LedLayout>(&mut self, layout: &L, led_idx: usize, timer_ms: u32) -> bool {
        let primary = self.primary.record_hit(layout, led_idx, timer_ms);
        let overlay = self
            .overlay
            .as_mut()
            .is_some_and(|effect| effect.record_hit(layout, led_idx, timer_ms));
        primary || overlay
    }
}

fn ripple_rng(seed: u64) -> Pcg32 {
    Pcg32::new(seed, RIPPLE_RNG_STREAM)
}

fn rain_rng(seed: u64) -> Pcg32 {
    Pcg32::new(seed, RAIN_RNG_STREAM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;
    use crate::palette::CARNIVAL;

    fn params(timer_ms: u32) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed: 128,
            sat: 255,
            val: 255,
            timer_ms,
        }
    }

    #[test]
    fn new_effects_append_without_renumbering_existing_effects() {
        assert_eq!(
            Effect::<1>::NAMES[Effect::<1>::CROSSHAIR_INDEX as usize],
            "Crosshair"
        );
        assert_eq!(
            Effect::<1>::NAMES[Effect::<1>::TRACER_INDEX as usize],
            "Tracer"
        );
        assert_eq!(
            Effect::<1>::NAMES[Effect::<1>::KEYFALL_INDEX as usize],
            "Keyfall"
        );
        assert_eq!(
            Effect::<1>::NAMES[Effect::<1>::SHOCKWAVE_INDEX as usize],
            "Shockwave"
        );
        for index in 0..Effect::<1>::NAMES.len() as u8 {
            assert_eq!(Effect::<1>::from_index(index, 1).unwrap().index(), index);
        }
    }

    #[test]
    fn every_typing_reactive_effect_accepts_key_hits() {
        const LEDS: &[(u8, u8)] = &[(128, 128)];
        let layout = SliceLayout::new(LEDS);
        for index in [
            6,
            Effect::<1>::CROSSHAIR_INDEX,
            Effect::<1>::TRACER_INDEX,
            Effect::<1>::KEYFALL_INDEX,
            Effect::<1>::SHOCKWAVE_INDEX,
        ] {
            let mut effect = Effect::<1>::from_index(index, 1).unwrap();
            assert!(
                effect.record_hit(&layout, 0, 0),
                "effect {index} ignored a hit"
            );
        }
    }

    /// Reactive can occupy the overlay slot above Rain and light a key whose
    /// rain background is dark.
    #[test]
    fn reactive_blends_over_dark_rain_background() {
        // x=200: the Rain RNG under this seed never places a drop close
        // enough to this column during the frames we sample.
        const LEDS: &[(u8, u8)] = &[(200, 128)];
        let layout = SliceLayout::new(LEDS);
        let mut scratch = [Hsv::default(); 1];
        let mut quiet_out = [Rgb::default(); 1];
        let mut hit_out = [Rgb::default(); 1];

        let build = || -> EffectStack<8> {
            EffectStack::new(
                Effect::<8>::from_index(Effect::<8>::RAIN_INDEX, 1).unwrap(),
                Some(Effect::<8>::from_index(6, 2).unwrap()),
            )
        };
        let mut quiet = build();
        let mut hit = build();
        assert!(hit.record_hit(&layout, 0, 0));

        let mut hit_lit = false;
        for t in (0..2000).step_by(50) {
            quiet.tick(&layout, params(t), &mut scratch, &mut quiet_out);
            assert_eq!(
                quiet_out[0],
                Rgb::default(),
                "idle overlay at t={t} must stay transparent"
            );
            hit.tick(&layout, params(t), &mut scratch, &mut hit_out);
            hit_lit |= hit_out[0] != quiet_out[0];
        }
        assert!(hit_lit, "key hit never changed its LED");
    }

    #[test]
    fn any_effect_can_fill_either_slot() {
        const LEDS: &[(u8, u8)] = &[(128, 128)];
        let layout = SliceLayout::new(LEDS);
        let mut scratch = [Hsv::default(); 1];
        let mut out = [Rgb::default(); 1];

        for primary in 0..Effect::<1>::NAMES.len() as u8 {
            for overlay in 0..Effect::<1>::NAMES.len() as u8 {
                let mut stack: EffectStack<1> = EffectStack::new(
                    Effect::<1>::from_index(primary, 1).unwrap(),
                    Some(Effect::<1>::from_index(overlay, 2).unwrap()),
                );
                stack.tick(&layout, params(100), &mut scratch, &mut out);
            }
        }
    }
}
