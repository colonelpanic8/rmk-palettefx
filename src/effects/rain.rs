//! RAIN: particles falling down the board, each with a fading trail.
//!
//! How busy it looks is set by [`RainParams`]: the spawn cadence and the
//! number of slots together decide how many drops are airborne, from a few
//! sparse streaks to a downpour. Each drop owns a column (a random
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

/// Drop slots the state always carries. [`RainParams::drops`] caps how many
/// of them spawn, so the array is sized for the largest allowed setting.
/// Rendering costs one bounds check per slot per LED, which at this size is
/// still trivial next to the per-frame palette work.
const RAIN_DROPS: usize = 32;

/// A spawn deadline further in the past than this restarts from the current
/// time instead of firing every slot at once. Without it, switching to Rain
/// after the clock has run on would dump the whole array in one frame and
/// the drops would fall in lockstep until they aged out.
const CADENCE_RESYNC_MS: u32 = 2_000;

/// Runtime tuning for the Rain effect (and for the rain background of
/// Storm). Every field is a `u8` so the whole struct maps directly onto a
/// host-visible parameter list; [`RainParams::DEFAULT`] reproduces the
/// values a board boots with when it has no persisted selection.
///
/// Values are validated by [`RainParams::set`] against the `*_MIN`/`*_MAX`
/// bounds below; the renderer assumes an in-range struct (in particular
/// non-zero `trail_len` and `column_half_width`, which it divides by).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RainParams {
    /// Drop slots allowed to spawn, 1..=32. How many are actually airborne
    /// is the spawn cadence times the fall time, so this is the ceiling
    /// rather than the count.
    pub drops: u8,
    /// Base delay between spawns, in units of 10 ms (so 30 = 300 ms). Jitter
    /// of up to half this delay is added per spawn so drops don't fall in
    /// lockstep; being proportional, it breaks up a slow cadence without
    /// putting a floor under a fast one.
    pub spawn_interval: u8,
    /// Trail length behind the head, in 0..=255 y-grid units.
    pub trail_len: u8,
    /// Half-width of the column a drop illuminates, in 0..=255 x-grid units.
    /// On a Glove80 (~10 columns per half mapped across 0..=255) the default
    /// covers roughly one key column with soft edges on the neighbours.
    pub column_half_width: u8,
}

impl RainParams {
    /// Milliseconds per unit of [`Self::spawn_interval`].
    pub const SPAWN_INTERVAL_STEP_MS: u32 = 10;

    pub const DROPS_MIN: u8 = 1;
    pub const DROPS_MAX: u8 = RAIN_DROPS as u8;
    pub const SPAWN_INTERVAL_MIN: u8 = 1;
    pub const SPAWN_INTERVAL_MAX: u8 = 255;
    pub const TRAIL_LEN_MIN: u8 = 16;
    pub const TRAIL_LEN_MAX: u8 = 255;
    pub const COLUMN_HALF_WIDTH_MIN: u8 = 4;
    pub const COLUMN_HALF_WIDTH_MAX: u8 = 64;

    /// The tuned defaults. Also the values advertised as each parameter's
    /// `default` to hosts.
    pub const DEFAULT: Self = Self {
        drops: 6,
        spawn_interval: 30,
        trail_len: 128,
        column_half_width: 14,
    };

    /// Number of parameters, i.e. the valid range of the indices accepted by
    /// [`Self::get`] and [`Self::set`].
    pub const COUNT: u8 = 4;

    /// Host-facing parameter names, in index order. `Spawn` carries its
    /// encoding in the name because the value is not milliseconds.
    pub const NAMES: [&'static str; Self::COUNT as usize] =
        ["Drops", "Spawn x10ms", "Trail", "Width"];

    /// Inclusive lower bound of each parameter, in index order.
    pub const MINS: [u8; Self::COUNT as usize] = [
        Self::DROPS_MIN,
        Self::SPAWN_INTERVAL_MIN,
        Self::TRAIL_LEN_MIN,
        Self::COLUMN_HALF_WIDTH_MIN,
    ];

    /// Inclusive upper bound of each parameter, in index order.
    pub const MAXES: [u8; Self::COUNT as usize] = [
        Self::DROPS_MAX,
        Self::SPAWN_INTERVAL_MAX,
        Self::TRAIL_LEN_MAX,
        Self::COLUMN_HALF_WIDTH_MAX,
    ];

    /// Default of each parameter, in index order.
    pub const DEFAULTS: [u8; Self::COUNT as usize] = [
        Self::DEFAULT.drops,
        Self::DEFAULT.spawn_interval,
        Self::DEFAULT.trail_len,
        Self::DEFAULT.column_half_width,
    ];

    /// Value of one parameter, or `None` when `index` is out of range.
    pub const fn get(&self, index: u8) -> Option<u8> {
        Some(match index {
            0 => self.drops,
            1 => self.spawn_interval,
            2 => self.trail_len,
            3 => self.column_half_width,
            _ => return None,
        })
    }

    /// Set one parameter. Returns `false` — leaving `self` untouched — when
    /// `index` is out of range or `value` falls outside that parameter's
    /// advertised bounds.
    pub const fn set(&mut self, index: u8, value: u8) -> bool {
        let i = index as usize;
        if i >= Self::COUNT as usize || value < Self::MINS[i] || value > Self::MAXES[i] {
            return false;
        }
        match index {
            0 => self.drops = value,
            1 => self.spawn_interval = value,
            2 => self.trail_len = value,
            3 => self.column_half_width = value,
            _ => return false,
        }
        true
    }

    /// Base spawn delay in milliseconds.
    const fn spawn_interval_ms(&self) -> u32 {
        self.spawn_interval as u32 * Self::SPAWN_INTERVAL_STEP_MS
    }

    /// One spawn delay: the base plus up to half of it, chosen from `entropy`.
    /// Scaling the jitter with the base keeps drops out of lockstep at every
    /// cadence; a fixed spread would swamp a short interval and cap how
    /// densely the board can rain.
    const fn spawn_delay_ms(&self, entropy: u8) -> u32 {
        let base = self.spawn_interval_ms();
        base + (base * entropy as u32) / 512
    }

    /// Drop slots that may spawn, clamped into the supported range so a
    /// hand-built struct cannot index past the drop array.
    const fn active_drops(&self) -> usize {
        if self.drops < Self::DROPS_MIN {
            Self::DROPS_MIN as usize
        } else if self.drops > Self::DROPS_MAX {
            RAIN_DROPS
        } else {
            self.drops as usize
        }
    }
}

impl Default for RainParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

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
    params: RainParams,
}

impl Default for RainState {
    fn default() -> Self {
        Self::new()
    }
}

impl RainState {
    pub const fn new() -> Self {
        Self::with_params(RainParams::DEFAULT)
    }

    /// State pre-tuned with `params`; see [`RainState::set_params`] for
    /// changing them on a running effect.
    pub const fn with_params(params: RainParams) -> Self {
        Self {
            drops: [Drop {
                spawn_ms: 0,
                x: 0,
                active: false,
            }; RAIN_DROPS],
            drops_tail: 0,
            next_spawn_ms: 0,
            initialized: false,
            params,
        }
    }

    /// Retune a running effect. Drops already in flight keep falling with
    /// the new trail and column geometry; lowering the drop count simply
    /// stops the surplus slots from respawning once they expire.
    pub const fn set_params(&mut self, params: RainParams) {
        self.params = params;
        let active = params.active_drops();
        if self.drops_tail >= active {
            self.drops_tail = 0;
        }
    }

    /// The tuning currently in effect.
    pub const fn params(&self) -> RainParams {
        self.params
    }

    /// Airborne drops; lets a test assert density without inferring it from
    /// lit LEDs, which a shared column would undercount.
    #[cfg(test)]
    fn active_drop_count(&self) -> usize {
        self.drops.iter().filter(|d| d.active).count()
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
        let active_drops = self.params.active_drops();
        let trail_len = self.params.trail_len.max(1) as u16;
        let col_half_width = self.params.column_half_width.max(1) as u16;
        // The head starts this far above y=0 and the drop dies once the whole
        // trail has cleared y=255, so drops fade in and out at the board edges.
        let overshoot = trail_len as i32;

        if !self.initialized {
            self.initialized = true;
            self.next_spawn_ms = params.timer_ms;
        }

        // Spawn a new drop if the slot at `drops_tail` is free and the
        // inter-drop timer has elapsed (wraparound-safe, as in Ripple).
        let due = |deadline: u32| params.timer_ms.wrapping_sub(deadline) < u32::MAX / 2;

        // A cadence shorter than the frame interval owes more than one drop
        // per tick, so spawn until the backlog is cleared instead of once per
        // frame -- otherwise the frame rate, not the parameter, sets the
        // ceiling on density. Each pass fills one slot, so `active_drops`
        // bounds the loop.
        if due(self.next_spawn_ms)
            && params.timer_ms.wrapping_sub(self.next_spawn_ms) > CADENCE_RESYNC_MS
        {
            self.next_spawn_ms = params.timer_ms;
        }
        for _ in 0..active_drops {
            if self.drops[self.drops_tail].active || !due(self.next_spawn_ms) {
                break;
            }
            let slot = self.drops_tail;
            self.drops[slot] = Drop {
                spawn_ms: params.timer_ms,
                x: rng(),
                active: true,
            };
            self.drops_tail = (slot + 1) % active_drops;
            // Advance from the deadline, not from now, so a cadence finer
            // than the frame interval keeps its average rate.
            self.next_spawn_ms = self
                .next_spawn_ms
                .wrapping_add(self.params.spawn_delay_ms(rng()).max(1));
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
            let y = tick / 2 - overshoot;
            if y > 255 + overshoot {
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
                if dx >= col_half_width {
                    continue;
                }
                // Distance behind the head: 0 at the head itself, growing
                // as the head falls past this LED. LEDs the head hasn't
                // reached yet stay dark.
                let behind = head - ly as i32;
                if !(0..=trail_len as i32).contains(&behind) {
                    continue;
                }
                let along = 255 - (behind as u16 * 255 / trail_len) as u8;
                let across = 255 - (dx * 255 / col_half_width) as u8;
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

    /// The advertised defaults must reproduce the tuning that used to be
    /// compiled in, so adding the parameters changed nothing on a board
    /// that never touches them.
    #[test]
    fn defaults_match_the_previously_hardcoded_tuning() {
        let params = RainParams::DEFAULT;
        assert_eq!(params.drops, 6);
        assert_eq!(params.spawn_interval_ms(), 300);
        assert_eq!(params.trail_len, 128);
        assert_eq!(params.column_half_width, 14);
        assert_eq!(RainState::new().params(), params);
        assert_eq!(
            RainParams::DEFAULTS,
            [
                params.drops,
                params.spawn_interval,
                params.trail_len,
                params.column_half_width
            ]
        );
    }

    #[test]
    fn out_of_range_parameters_are_declined_without_mutating() {
        let mut params = RainParams::DEFAULT;
        assert!(!params.set(0, RainParams::DROPS_MAX + 1));
        assert!(!params.set(2, RainParams::TRAIL_LEN_MIN - 1));
        assert!(!params.set(3, RainParams::COLUMN_HALF_WIDTH_MAX + 1));
        assert!(!params.set(RainParams::COUNT, 1));
        assert_eq!(params, RainParams::DEFAULT);
        assert_eq!(params.get(RainParams::COUNT), None);

        assert!(params.set(2, RainParams::TRAIL_LEN_MIN));
        assert_eq!(params.get(2), Some(RainParams::TRAIL_LEN_MIN));
    }

    /// Peak simultaneously lit LEDs over a time sweep, a stand-in for "how
    /// much of the board the effect covers at once".
    fn peak_lit<const N: usize>(
        state: &mut RainState,
        layout: &SliceLayout<'_>,
        mut rng: impl FnMut() -> u8,
    ) -> usize {
        let mut out = [Hsv::default(); N];
        let mut peak = 0;
        for t in (0..8000).step_by(20) {
            state.tick(layout, params(t, 128), &mut rng, &mut out);
            peak = peak.max(lit(&out));
        }
        peak
    }

    /// A longer trail keeps LEDs the head has already passed lit, so more of
    /// a sparse column glows at once.
    #[test]
    fn trail_length_parameter_changes_how_much_of_a_column_glows() {
        let layout = SliceLayout::new(COLUMN);

        let mut short = RainState::with_single_drop(0, 0);
        short.set_params(RainParams {
            trail_len: RainParams::TRAIL_LEN_MIN,
            ..RainParams::DEFAULT
        });
        // The LEDs are 64 grid units apart, so a 16-unit trail never spans
        // two of them.
        assert_eq!(peak_lit::<5>(&mut short, &layout, || 0), 1);

        let mut long = RainState::with_single_drop(0, 0);
        long.set_params(RainParams {
            trail_len: RainParams::TRAIL_LEN_MAX,
            ..RainParams::DEFAULT
        });
        assert!(peak_lit::<5>(&mut long, &layout, || 0) > 1);
    }

    /// The point of the ceiling being 32 rather than a handful: a fast cadence
    /// has to actually fill the board. The frame interval must not cap it
    /// either -- at 10 ms between spawns a 40 ms tick owes four drops, so
    /// spawning once per tick would silently pin density to the frame rate.
    #[test]
    fn a_cadence_finer_than_the_tick_still_fills_every_slot() {
        const POS: &[(u8, u8)] = &[(0, 0)];
        let layout = SliceLayout::new(POS);
        let mut out = [Hsv::default(); 1];

        let mut state = RainState::with_params(RainParams {
            drops: RainParams::DROPS_MAX,
            spawn_interval: 1, // 10 ms, far below the 40 ms frame interval
            ..RainParams::DEFAULT
        });

        // Two 40 ms frames: eight spawns are owed, so a once-per-tick spawner
        // would have managed two.
        state.tick(&layout, params(0, 128), || 0, &mut out);
        state.tick(&layout, params(40, 128), || 0, &mut out);
        state.tick(&layout, params(80, 128), || 0, &mut out);
        assert!(
            state.active_drop_count() >= 8,
            "expected the backlog to be spawned, got {}",
            state.active_drop_count()
        );

        // The ceiling still holds.
        for t in (120..4000).step_by(40) {
            state.tick(&layout, params(t, 128), || 0, &mut out);
            assert!(state.active_drop_count() <= RainParams::DROPS_MAX as usize);
        }
    }

    /// Jitter scales with the cadence instead of being a fixed spread, so a
    /// short interval is not swamped by it. A fixed 0..510 ms spread put an
    /// unavoidable floor of a quarter second under the average gap.
    #[test]
    fn spawn_jitter_stays_proportional_to_the_cadence() {
        let fast = RainParams {
            spawn_interval: 1,
            ..RainParams::DEFAULT
        };
        assert_eq!(fast.spawn_delay_ms(0), 10);
        assert!(
            fast.spawn_delay_ms(255) <= 15,
            "jitter must not swamp 10 ms"
        );

        let slow = RainParams {
            spawn_interval: 100,
            ..RainParams::DEFAULT
        };
        assert_eq!(slow.spawn_delay_ms(0), 1000);
        assert!(
            slow.spawn_delay_ms(255) > 1200,
            "a slow cadence still needs real spread"
        );
    }

    /// Raising the drop count puts more independent columns in flight.
    #[test]
    fn drop_count_parameter_bounds_concurrent_drops() {
        // One LED per drop column, far enough apart that a drop lights only
        // its own.
        const ROW: &[(u8, u8)] = &[
            (0, 128),
            (32, 128),
            (64, 128),
            (96, 128),
            (128, 128),
            (160, 128),
            (192, 128),
            (224, 128),
        ];
        let layout = SliceLayout::new(ROW);
        // Each spawn draws two bytes: the column, then the jitter. Walk the
        // columns and keep the jitter at zero so the drop count is the only
        // limit on how many drops are in flight.
        let column_walk = || {
            let mut draws = 0u32;
            move || {
                draws += 1;
                if draws % 2 == 1 {
                    (draws / 2 % 8) as u8 * 32
                } else {
                    0
                }
            }
        };

        let mut sparse = RainState::new();
        sparse.set_params(RainParams {
            drops: RainParams::DROPS_MIN,
            spawn_interval: 1,
            ..RainParams::DEFAULT
        });
        let sparse_peak = peak_lit::<8>(&mut sparse, &layout, column_walk());

        let mut dense = RainState::new();
        dense.set_params(RainParams {
            drops: RainParams::DROPS_MAX,
            spawn_interval: 1,
            ..RainParams::DEFAULT
        });
        let dense_peak = peak_lit::<8>(&mut dense, &layout, column_walk());

        assert_eq!(sparse_peak, 1);
        assert!(
            dense_peak > sparse_peak,
            "eight drops lit {dense_peak} LEDs, one drop lit {sparse_peak}"
        );
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
