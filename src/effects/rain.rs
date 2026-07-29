//! RAIN: sparse particles falling down the board, each with a fading trail.
//!
//! A few drops are active at any time. Each drop owns a column (a random
//! x position) and a head that sweeps from above the board to below it;
//! LEDs near the head light at full palette value and LEDs the head has
//! already passed form a trail that decays with distance behind the head,
//! giving a matrix-rain look. LEDs outside every drop's column stay dark.

use rand_core::Rng;

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::math::{scale8, scale16by8};
use crate::palette::interp_color;

/// Active drops. Kept small so the board reads as sparse: with the spawn
/// cadence below, typically 3-5 drops are visible at once.
const RAIN_DROPS: usize = 6;
/// Base delay between spawns; a per-spawn random jitter of 0..=510 ms is
/// added on top so drops don't fall in lockstep.
const RAIN_SPAWN_INTERVAL_MS: u32 = 300;
/// Trail length behind the head, in 0..=255 y-grid units.
const TRAIL_LEN: u16 = 128;
/// Half-width of the column a drop illuminates, in 0..=255 x-grid units.
/// On a Glove80 (~10 columns per half mapped across 0..=255) this covers
/// roughly one key column with soft edges on the neighbours.
const COL_HALF_WIDTH: u16 = 14;
/// The head starts this far above y=0 and the drop dies once the whole
/// trail has cleared y=255, so drops fade in and out at the board edges.
const OVERSHOOT: i32 = TRAIL_LEN as i32;

#[derive(Copy, Clone, Default)]
struct Drop {
    /// Timer value (millisecond counter, low 32 bits) when the drop spawned.
    spawn_ms: u32,
    x: u8,
    active: bool,
}

pub struct RainState {
    drops: [Drop; RAIN_DROPS],
    drops_tail: usize,
    next_spawn_ms: u32,
    initialized: bool,
}

impl Default for RainState {
    fn default() -> Self {
        Self::new()
    }
}

impl RainState {
    pub const fn new() -> Self {
        Self {
            drops: [Drop {
                spawn_ms: 0,
                x: 0,
                active: false,
            }; RAIN_DROPS],
            drops_tail: 0,
            next_spawn_ms: 0,
            initialized: false,
        }
    }

    /// State with exactly one drop and spawning suppressed, so tests can
    /// observe a single trail without other drops overlapping it.
    #[cfg(test)]
    fn with_single_drop(x: u8, spawn_ms: u32) -> Self {
        let mut state = Self::new();
        state.initialized = true;
        state.drops[0] = Drop {
            spawn_ms,
            x,
            active: true,
        };
        state.drops_tail = 0;
        state.next_spawn_ms = spawn_ms.wrapping_add(1_000_000);
        state
    }

    /// Tick the rain effect. `rng()` is called when a new drop is spawned
    /// and should return bytes uniformly over 0..=255; one byte picks the
    /// drop's x position, another the spawn jitter.
    pub fn tick<L, R>(&mut self, layout: &L, params: FrameParams<'_>, rng: R, out: &mut [Hsv])
    where
        L: LedLayout,
        R: FnMut() -> u8,
    {
        self.tick_blend(layout, params, rng, out, |_, _, _| 0);
    }

    /// Like [`Self::tick`], but blends in an extra per-LED intensity from
    /// `extra(led_index, lx, ly)`: the final palette lookup uses the max of
    /// the rain intensity and the extra intensity. Lets another effect (e.g.
    /// Reactive's key-hit bumps) render over the rain background while
    /// keeping unlit LEDs black.
    pub fn tick_blend<L, R, F>(
        &mut self,
        layout: &L,
        params: FrameParams<'_>,
        mut rng: R,
        out: &mut [Hsv],
        mut extra: F,
    ) where
        L: LedLayout,
        R: FnMut() -> u8,
        F: FnMut(usize, u8, u8) -> u8,
    {
        let count = layout.count();

        if !self.initialized {
            self.initialized = true;
            self.next_spawn_ms = params.timer_ms;
        }

        // Spawn a new drop if the slot at `drops_tail` is free and the
        // inter-drop timer has elapsed (wraparound-safe, as in Ripple).
        if !self.drops[self.drops_tail].active
            && params.timer_ms.wrapping_sub(self.next_spawn_ms) < u32::MAX / 2
        {
            let slot = self.drops_tail;
            self.drops[slot] = Drop {
                spawn_ms: params.timer_ms,
                x: rng(),
                active: true,
            };
            self.drops_tail = (slot + 1) % RAIN_DROPS;
            self.next_spawn_ms = params
                .timer_ms
                .wrapping_add(RAIN_SPAWN_INTERVAL_MS + (rng() as u32) * 2);
        }

        // Head y-position per drop, in an extended coordinate so the head
        // enters from above y=0 and the trail exits past y=255.
        let mut heads = [0i32; RAIN_DROPS];
        for (drop, head) in self.drops.iter_mut().zip(heads.iter_mut()) {
            if !drop.active {
                continue;
            }
            let elapsed = params.timer_ms.wrapping_sub(drop.spawn_ms);
            if elapsed > u16::MAX as u32 {
                drop.active = false;
                continue;
            }
            // ~2x Ripple's expansion rate so drops read as falling rather
            // than drifting; speed=128 crosses the board in about a second.
            let tick = scale16by8(elapsed as u16, 1 + params.speed / 4) as i32;
            let y = tick / 2 - OVERSHOOT;
            if y > 255 + OVERSHOOT {
                drop.active = false;
                continue;
            }
            *head = y;
        }

        for (i, slot) in out.iter_mut().take(count).enumerate() {
            let (lx, ly) = layout.position(i);
            let mut intensity = 0u8;

            for (drop, &head) in self.drops.iter().zip(heads.iter()) {
                if !drop.active {
                    continue;
                }
                let dx = if lx > drop.x {
                    lx - drop.x
                } else {
                    drop.x - lx
                } as u16;
                if dx >= COL_HALF_WIDTH {
                    continue;
                }
                // Distance behind the head: 0 at the head itself, growing
                // as the head falls past this LED. LEDs the head hasn't
                // reached yet stay dark.
                let behind = head - ly as i32;
                if !(0..=TRAIL_LEN as i32).contains(&behind) {
                    continue;
                }
                let along = 255 - (behind as u16 * 255 / TRAIL_LEN) as u8;
                let across = 255 - (dx * 255 / COL_HALF_WIDTH) as u8;
                intensity = intensity.max(scale8(along, across));
            }

            intensity = intensity.max(extra(i, lx, ly));

            // Sample the palette by intensity (head = top of the palette)
            // and also scale luminance by it, so the background is black
            // and the trail dims toward its tail.
            *slot = interp_color(
                params.palette,
                intensity,
                params.sat,
                scale8(intensity, params.val),
            );
        }
    }
}

impl RainState {
    /// Convenience wrapper around [`RainState::tick`] that accepts any
    /// [`Rng`] for drop placement, sampling `rng.next_u32() as u8` per call.
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

    fn params(timer_ms: u32, speed: u8) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed,
            sat: 255,
            val: 255,
            timer_ms,
        }
    }

    // A single column of LEDs; the RNG always returns 0 so the drop's
    // column is x=0, aligned with every LED.
    const COLUMN: &[(u8, u8)] = &[(0, 0), (0, 64), (0, 128), (0, 192), (0, 255)];

    fn lit(frame: &[Hsv]) -> usize {
        frame.iter().filter(|h| h.v != 0).count()
    }

    #[test]
    fn drop_sweeps_down_its_column_with_a_trail() {
        let layout = SliceLayout::new(COLUMN);
        let mut state = RainState::new();
        let mut out = [Hsv::default(); 5];

        // Frame 0 spawns a drop with its head above the board: all dark.
        state.tick(&layout, params(0, 128), || 0, &mut out);
        assert_eq!(lit(&out), 0);

        // Mid-fall the head is on the board and a trail follows it.
        let mut saw_partial = false;
        for t in (100..3000).step_by(100) {
            state.tick(&layout, params(t, 128), || 0, &mut out);
            let n = lit(&out);
            if n > 0 && n < COLUMN.len() {
                saw_partial = true;
            }
        }
        assert!(saw_partial, "expected a partial trail at some point");
    }

    #[test]
    fn head_is_brighter_than_trail() {
        let layout = SliceLayout::new(COLUMN);
        let mut state = RainState::with_single_drop(0, 0);
        let mut out = [Hsv::default(); 5];

        // Walk time until at least two LEDs are lit, then check ordering:
        // the lower (later-reached) LED is the head and must be brighter.
        for t in (0..5000).step_by(50) {
            state.tick(&layout, params(t, 128), || 0, &mut out);
            let first = (0..out.len()).find(|&i| out[i].v != 0);
            let last = (0..out.len()).rev().find(|&i| out[i].v != 0);
            if let (Some(tail), Some(head)) = (first, last) {
                if head != tail {
                    assert!(out[head].v > out[tail].v);
                    return;
                }
            }
        }
        panic!("drop never lit two LEDs");
    }

    #[test]
    fn leds_off_the_drop_column_stay_dark() {
        const OFF_COLUMN: &[(u8, u8)] = &[(200, 0), (200, 128), (200, 255)];
        let layout = SliceLayout::new(OFF_COLUMN);
        let mut state = RainState::new();
        let mut out = [Hsv::default(); 3];

        for t in (0..5000).step_by(50) {
            state.tick(&layout, params(t, 128), || 0, &mut out);
            assert_eq!(lit(&out), 0);
        }
    }
}
