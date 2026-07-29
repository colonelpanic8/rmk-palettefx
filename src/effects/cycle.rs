//! Runtime-switchable effect selection.
//!
//! Firmware that cycles through the PaletteFx effects at runtime (RGB
//! mode-forward/backward keys) needs one value that owns whichever effect
//! is active plus its state. [`Effect`] is that value: cycling resets the
//! outgoing effect's state so each effect starts from its own time phase.

use super::{
    FlowState, FrameParams, Pcg32, RainParams, RainState, ReactiveState, RippleState, SparkleState,
    VortexState,
};
use crate::color::Hsv;
use crate::layout::LedLayout;
use rand_core::Rng;

/// Stream selector for the Ripple RNG; any odd constant works, this one is
/// from the PCG reference implementation.
const RIPPLE_RNG_STREAM: u64 = 0xA02B_DBF7_BB3C_0A7F;

/// Stream selector for the Rain RNG; distinct from Ripple's so the two
/// effects don't share drop-placement sequences when seeded alike.
const RAIN_RNG_STREAM: u64 = 0x5851_F42D_4C95_7F2D;

/// The currently active effect and its state. `HITS` sizes the Reactive
/// effect's ring of remembered key presses.
pub enum Effect<const HITS: usize> {
    Gradient,
    Flow(FlowState),
    Vortex(VortexState),
    Sparkle(SparkleState),
    Ripple(RippleState, Pcg32),
    Rain(RainState, Pcg32),
    Reactive(ReactiveState<HITS>),
    /// Rain as the ambient background with Reactive's key-hit bumps
    /// blended over it.
    Storm(RainState, Pcg32, ReactiveState<HITS>),
}

impl<const HITS: usize> Effect<HITS> {
    /// Display names in stable index order; `index`/`from_index` and the
    /// next/prev cycle all agree with this ordering.
    pub const NAMES: [&'static str; 8] = [
        "Gradient", "Flow", "Vortex", "Sparkle", "Ripple", "Rain", "Reactive", "Storm",
    ];

    /// Stable index of the Rain effect into [`Self::NAMES`].
    pub const RAIN_INDEX: u8 = 5;
    /// Stable index of the Storm effect into [`Self::NAMES`].
    pub const STORM_INDEX: u8 = 7;

    /// Whether the effect at `index` renders a [`RainState`], and therefore
    /// exposes the [`RainParams`] tuning. Rain and Storm both do; Storm's
    /// background *is* the rain, so it takes the same knobs.
    pub const fn uses_rain_params(index: u8) -> bool {
        matches!(index, Self::RAIN_INDEX | Self::STORM_INDEX)
    }

    /// Retune the rain of whichever variant owns a [`RainState`]; a no-op
    /// for every other effect.
    pub const fn set_rain_params(&mut self, params: RainParams) {
        match self {
            Self::Rain(rain, _) | Self::Storm(rain, _, _) => rain.set_params(params),
            _ => {}
        }
    }

    /// The rain tuning of whichever variant owns a [`RainState`], or `None`
    /// for effects that have none.
    pub const fn rain_params(&self) -> Option<RainParams> {
        match self {
            Self::Rain(rain, _) | Self::Storm(rain, _, _) => Some(rain.params()),
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
            Self::Storm(_, _, _) => 7,
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
            7 => Self::Storm(
                RainState::new(),
                rain_rng(ripple_seed),
                ReactiveState::new(),
            ),
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
            Self::Storm(rain, rng, reactive) => {
                let amps = reactive.amplitudes(params);
                let reactive = &*reactive;
                rain.tick_blend(
                    layout,
                    params,
                    || rng.next_u32() as u8,
                    out,
                    |_, lx, ly| reactive.value_at(&amps, lx, ly),
                );
            }
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
            Self::Reactive(_) => Self::Storm(
                RainState::new(),
                rain_rng(ripple_seed),
                ReactiveState::new(),
            ),
            Self::Storm(_, _, _) => Self::Gradient,
        };
    }

    /// Switch to the previous effect in the cycle; see [`Effect::next`].
    pub fn prev(&mut self, ripple_seed: u64) {
        *self = match self {
            Self::Gradient => Self::Storm(
                RainState::new(),
                rain_rng(ripple_seed),
                ReactiveState::new(),
            ),
            Self::Storm(_, _, _) => Self::Reactive(ReactiveState::new()),
            Self::Reactive(_) => Self::Rain(RainState::new(), rain_rng(ripple_seed)),
            Self::Rain(_, _) => Self::Ripple(RippleState::new(), ripple_rng(ripple_seed)),
            Self::Ripple(_, _) => Self::Sparkle(SparkleState::new()),
            Self::Sparkle(_) => Self::Vortex(VortexState::new()),
            Self::Vortex(_) => Self::Flow(FlowState::new()),
            Self::Flow(_) => Self::Gradient,
        };
    }

    /// Record a key press at `led_idx` against the Reactive effect; no-op
    /// for every other variant. Returns `true` iff a hit was recorded, so
    /// the caller can skip a re-render that would not be visible.
    pub fn record_hit<L: LedLayout>(&mut self, layout: &L, led_idx: usize, timer_ms: u32) -> bool {
        let s = match self {
            Self::Reactive(s) => s,
            Self::Storm(_, _, s) => s,
            _ => return false,
        };
        let (x, y) = layout.position(led_idx);
        s.record_hit(x, y, timer_ms);
        true
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

    /// In Storm, a key hit lights its LED even when no rain drop is in
    /// that column, and without hits the same LED stays dark (rain keeps
    /// its black background rather than Reactive's dim ambient one).
    #[test]
    fn storm_blends_key_hits_over_dark_rain_background() {
        // x=200: the Storm RNG under this seed never places a drop close
        // enough to this column during the frames we sample.
        const LEDS: &[(u8, u8)] = &[(200, 128)];
        let layout = SliceLayout::new(LEDS);
        let mut out = [Hsv::default(); 1];

        let mut quiet: Effect<8> = Effect::from_index(7, 1).expect("Storm index");
        let mut hit: Effect<8> = Effect::from_index(7, 1).expect("Storm index");
        assert!(hit.record_hit(&layout, 0, 0));

        let mut hit_lit = false;
        for t in (0..2000).step_by(50) {
            quiet.tick(&layout, params(t), &mut out);
            assert_eq!(out[0].v, 0, "no drop and no hit at t={t} must be black");
            hit.tick(&layout, params(t), &mut out);
            hit_lit |= out[0].v != 0;
        }
        assert!(hit_lit, "key hit never lit its LED");
    }
}
