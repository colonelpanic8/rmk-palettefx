//! Runtime-switchable effect selection.
//!
//! Firmware that cycles through the PaletteFx effects at runtime (RGB
//! mode-forward/backward keys) needs one value that owns whichever effect
//! is active plus its state. [`Effect`] is that value: cycling resets the
//! outgoing effect's state so each effect starts from its own time phase.

use super::{FlowState, FrameParams, Pcg32, ReactiveState, RippleState, SparkleState, VortexState};
use crate::color::Hsv;
use crate::layout::LedLayout;

/// Stream selector for the Ripple RNG; any odd constant works, this one is
/// from the PCG reference implementation.
const RIPPLE_RNG_STREAM: u64 = 0xA02B_DBF7_BB3C_0A7F;

/// The currently active effect and its state. `HITS` sizes the Reactive
/// effect's ring of remembered key presses.
pub enum Effect<const HITS: usize> {
    Gradient,
    Flow(FlowState),
    Vortex(VortexState),
    Sparkle(SparkleState),
    Ripple(RippleState, Pcg32),
    Reactive(ReactiveState<HITS>),
}

impl<const HITS: usize> Effect<HITS> {
    /// Display names in stable index order; `index`/`from_index` and the
    /// next/prev cycle all agree with this ordering.
    pub const NAMES: [&'static str; 6] = [
        "Gradient", "Flow", "Vortex", "Sparkle", "Ripple", "Reactive",
    ];

    /// Stable index of the active effect into [`Self::NAMES`].
    pub const fn index(&self) -> u8 {
        match self {
            Self::Gradient => 0,
            Self::Flow(_) => 1,
            Self::Vortex(_) => 2,
            Self::Sparkle(_) => 3,
            Self::Ripple(_, _) => 4,
            Self::Reactive(_) => 5,
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
            5 => Self::Reactive(ReactiveState::new()),
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
            Self::Reactive(s) => s.tick(layout, params, out),
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
            Self::Ripple(_, _) => Self::Reactive(ReactiveState::new()),
            Self::Reactive(_) => Self::Gradient,
        };
    }

    /// Switch to the previous effect in the cycle; see [`Effect::next`].
    pub fn prev(&mut self, ripple_seed: u64) {
        *self = match self {
            Self::Gradient => Self::Reactive(ReactiveState::new()),
            Self::Reactive(_) => Self::Ripple(RippleState::new(), ripple_rng(ripple_seed)),
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
        let Self::Reactive(s) = self else {
            return false;
        };
        let (x, y) = layout.position(led_idx);
        s.record_hit(x, y, timer_ms);
        true
    }
}

fn ripple_rng(seed: u64) -> Pcg32 {
    Pcg32::new(seed, RIPPLE_RNG_STREAM)
}
