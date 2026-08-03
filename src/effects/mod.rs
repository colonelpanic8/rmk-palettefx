//! The PaletteFx effects. Each effect writes one [`Hsv`](crate::color::Hsv)
//! per LED into the caller-supplied output slice; converting to RGB and
//! pushing to the LED driver is the caller's responsibility.

use crate::palette::Palette;

mod crosshair;
mod cycle;
mod flow;
mod gradient;
mod keyfall;
mod rain;
mod reactive;
mod ripple;
mod shockwave;
mod sparkle;
mod tracer;
mod vortex;

pub use crosshair::{CrosshairMotion, CrosshairParams, CrosshairState};
pub use cycle::{Effect, EffectStack};
pub use flow::FlowState;
pub use gradient::gradient;
pub use keyfall::{KeyfallGravity, KeyfallParams, KeyfallState};
pub use rain::{RainParams, RainState};
pub use reactive::{Hit, ReactiveState};
pub use ripple::{Pcg32, RippleState};
pub use shockwave::ShockwaveState;
pub use sparkle::SparkleState;
pub use tracer::TracerState;
pub use vortex::VortexState;

/// Parameters shared by every effect.
#[derive(Copy, Clone, Debug)]
pub struct FrameParams<'p> {
    pub palette: &'p Palette,
    /// 0..=255, maps to PaletteFx's `rgb_matrix_config.speed`.
    pub speed: u8,
    /// Global saturation scale. Pass 255 to leave the palette unmodified.
    pub sat: u8,
    /// Global brightness/value scale. Pass 255 for full brightness.
    pub val: u8,
    /// Millisecond counter, wrapping every ~49 days. Normally monotonic, but
    /// not guaranteed to be: a renderer that shares an animation clock with
    /// another node re-anchors to it, so this can step backward. Effects that
    /// carry timestamps between frames must cope -- see [`clock_rewound`].
    pub timer_ms: u32,
}

/// A backward step larger than this is a re-anchored clock rather than noise.
///
/// Sharing an animation clock costs the transport's latency, and that latency
/// varies frame to frame, so the shared time routinely steps back a few
/// milliseconds. The threshold sits well above that jitter and well below any
/// interval a viewer would notice.
const CLOCK_REWIND_MS: u32 = 2_000;

/// Whether the animation clock jumped backward between `last_ms` and `now_ms`
/// by enough to invalidate timestamps taken against it.
///
/// This is not hypothetical: on a split keyboard the peripheral renders on the
/// central's clock, so its time snaps back to the central's uptime every time
/// the central reboots while the peripheral keeps running -- reflashing or
/// replugging one half is enough. An effect that spawns against a deadline
/// stalls for as wide as the jump unless it notices, because spawning only
/// ever pushes the deadline further forward.
///
/// Ordinary wrapping is not a rewind: a forward step across the u32 boundary
/// stays small under wrapping arithmetic, so only genuine backward motion is
/// reported.
pub(crate) fn clock_rewound(last_ms: u32, now_ms: u32) -> bool {
    let rewound = last_ms.wrapping_sub(now_ms);
    rewound > CLOCK_REWIND_MS && rewound < u32::MAX / 2
}
