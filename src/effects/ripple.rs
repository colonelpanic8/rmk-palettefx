//! RIPPLE: up to 3 radial drops with amplitude envelope.

use rand_core::Rng;

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::math::{abs_half_diff, cos8, ease8_in_out_approx, scale8, scale16by8, sqrt16};
use crate::palette::interp_color;

/// Re-export of the default RNG for [`RippleState::tick_with_rng`].
/// `Pcg32` is small and seedable; any [`Rng`](rand_core::Rng) impl
/// works as a substitute.
pub use rand_pcg::Pcg32;

const RIPPLE_DROPS: usize = 3;
const RIPPLE_SPAWN_INTERVAL_MS: u32 = 1000;

#[derive(Copy, Clone, Default)]
struct Droplet {
    /// Timer value (millisecond counter, low 32 bits) when the drop spawned.
    spawn_ms: u32,
    x: u8,
    y: u8,
    amplitude: u8,
    scale: u8,
    phase: u8,
}

pub struct RippleState {
    drops: [Droplet; RIPPLE_DROPS],
    drops_tail: usize,
    next_spawn_ms: u32,
    /// The animation clock at the previous frame, so a clock that jumps
    /// backward can be told from one that simply advanced.
    last_timer_ms: u32,
    initialized: bool,
}

impl Default for RippleState {
    fn default() -> Self {
        Self::new()
    }
}

impl RippleState {
    pub const fn new() -> Self {
        Self {
            drops: [Droplet {
                spawn_ms: 0,
                x: 0,
                y: 0,
                amplitude: 0,
                scale: 0,
                phase: 0,
            }; RIPPLE_DROPS],
            drops_tail: 0,
            next_spawn_ms: 0,
            last_timer_ms: 0,
            initialized: false,
        }
    }

    /// Tick the ripple effect. `rng()` is called whenever a new drop is
    /// spawned and should return a byte uniformly over 0..=255. The LED
    /// index `rng() % led_count` becomes the drop centre.
    pub fn tick<L, R>(&mut self, layout: &L, params: FrameParams<'_>, mut rng: R, out: &mut [Hsv])
    where
        L: LedLayout,
        R: FnMut() -> u8,
    {
        let count = layout.count();

        if !self.initialized {
            self.initialized = true;
            self.next_spawn_ms = params.timer_ms;
        }

        // The deadline is a reading of the animation clock, so a clock that
        // jumped backward leaves it stranded in a future that spawning can
        // only push further away. Bring it back to now; the droplets in
        // flight need no such rescue, since their envelope ends them on the
        // first frame whose elapsed time no longer makes sense.
        if super::clock_rewound(self.last_timer_ms, params.timer_ms) {
            self.next_spawn_ms = params.timer_ms;
        }
        self.last_timer_ms = params.timer_ms;

        // Spawn a new drop if the slot at `drops_tail` is free and the
        // inter-drop timer has elapsed.
        if self.drops[self.drops_tail].amplitude == 0
            && params.timer_ms.wrapping_sub(self.next_spawn_ms) < u32::MAX / 2
        {
            let led = (rng() as usize) % count.max(1);
            let (dx, dy) = layout.position(led);
            let slot = self.drops_tail;
            self.drops[slot] = Droplet {
                spawn_ms: params.timer_ms,
                x: dx,
                y: dy,
                amplitude: 1,
                scale: 0,
                phase: 0,
            };
            self.drops_tail = (slot + 1) % RIPPLE_DROPS;
            self.next_spawn_ms = params.timer_ms.wrapping_add(RIPPLE_SPAWN_INTERVAL_MS);
        }

        // Advance each active droplet.
        for droplet in &mut self.drops {
            if droplet.amplitude == 0 {
                continue;
            }
            let elapsed = params.timer_ms.wrapping_sub(droplet.spawn_ms) as u16;
            let tick = scale16by8(elapsed, 1 + params.speed / 4);
            if tick < 4 * 255 {
                let t = (tick / 4) as u8;
                droplet.amplitude = ripple_amplitude(t);
                droplet.scale = 255 / (1 + t / 2);
                droplet.phase = tick as u8;
            } else {
                droplet.amplitude = 0;
            }
        }

        // Render.
        for (i, slot) in out.iter_mut().take(count).enumerate() {
            let (lx, ly) = layout.position(i);
            let mut value: i16 = 128;

            for droplet in &self.drops {
                if droplet.amplitude == 0 {
                    continue;
                }
                let dx = abs_half_diff(lx, droplet.x);
                let dy = abs_half_diff(ly, droplet.y);
                let r = sqrt16((dx as u16) * (dx as u16) + (dy as u16) * (dy as u16));
                let r_scaled = (r as u16) * (droplet.scale as u16);

                if r_scaled < 255 {
                    let bump = scale8(ease8_in_out_approx(255 - r_scaled as u8), droplet.amplitude);
                    let wave = (cos8(8u8.wrapping_mul(r).wrapping_sub(droplet.phase)) as i16) - 128;
                    value += (wave * (bump as i16)) / 128;
                }
            }

            let value = value.clamp(0, 255) as u8;
            *slot = interp_color(params.palette, value, params.sat, params.val);
        }
    }
}

/// Droplet amplitude envelope: rising for t<32, plateau to t=55, then a smooth
/// quadratic-ish decay back to zero at t=255.
fn ripple_amplitude(t: u8) -> u8 {
    if t <= 55 {
        if t < 32 { 3 + 5 * t } else { 192 }
    } else {
        let u = (((255 - t) as u16) * 123) >> 7;
        scale8(u as u8, u as u8)
    }
}

impl RippleState {
    /// Convenience wrapper around [`RippleState::tick`] that accepts any
    /// [`Rng`] for drop placement, sampling `rng.next_u32() as u8` per spawn.
    pub fn tick_with_rng<L, R>(
        &mut self,
        rng: &mut R,
        layout: &L,
        params: FrameParams<'_>,
        out: &mut [Hsv],
    ) where
        L: LedLayout,
        R: Rng,
    {
        self.tick(layout, params, || rng.next_u32() as u8, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;
    use crate::palette::CARNIVAL;

    const POS: &[(u8, u8)] = &[(0, 0), (128, 128), (255, 255)];

    fn params(timer_ms: u32) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed: 128,
            sat: 255,
            val: 255,
            timer_ms,
        }
    }

    fn airborne(state: &RippleState) -> usize {
        state.drops.iter().filter(|d| d.amplitude != 0).count()
    }

    /// Spawns observed while ticking `state` across `times`. Counting spawns
    /// rather than lit droplets matters after a clock jump: `elapsed` is
    /// truncated to 16 bits, so a droplet already in flight can land back
    /// inside its envelope by chance and keep rendering. Its presence would
    /// say nothing about whether spawning still works.
    fn spawns_while_ticking(
        state: &mut RippleState,
        layout: &SliceLayout<'_>,
        times: impl Iterator<Item = u32>,
    ) -> usize {
        let mut out = [Hsv::default(); 3];
        let mut tail = state.drops_tail;
        let mut spawns = 0;
        for t in times {
            state.tick(layout, params(t), || 0, &mut out);
            if state.drops_tail != tail {
                spawns += 1;
                tail = state.drops_tail;
            }
        }
        spawns
    }

    /// The animation clock can move backward: a split peripheral renders on
    /// the central's clock, so its time snaps back to the central's uptime
    /// whenever the central reboots under it. A spawn only pushes the
    /// deadline forward, so an unrescued jump left ripple spawning nothing
    /// for as wide as the jump. Same defect as the one Rain carried.
    #[test]
    fn spawning_comes_back_after_the_clock_jumps_backward() {
        let layout = SliceLayout::new(POS);
        let mut out = [Hsv::default(); 3];
        let mut state = RippleState::new();

        // An hour of central uptime, replicated to the peripheral.
        const UPTIME_MS: u32 = 60 * 60 * 1000;
        state.tick(&layout, params(UPTIME_MS), || 0, &mut out);
        assert_eq!(airborne(&state), 1, "the first tick should spawn a drop");

        // The central reboots; the shared clock restarts from its uptime.
        let spawns = spawns_while_ticking(&mut state, &layout, (0..2_000).step_by(40));
        assert!(
            spawns > 0,
            "ripple never spawned again after the clock moved back"
        );
    }
}
