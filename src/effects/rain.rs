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

/// Runtime tuning for the Rain effect. Every field is a `u8` so the whole struct maps directly onto a
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
    /// Hue of the rain itself. Rain reads its brightness and saturation ramp
    /// from the palette but takes its hue from here, so the streaks stay one
    /// colour instead of sliding along the palette's gradient as a drop fades.
    /// 86 is green, 172 blue.
    pub rain_hue: u8,
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
    pub const HUE_MIN: u8 = 0;
    pub const HUE_MAX: u8 = 255;

    /// Pure hues under the spectrum conversion in [`crate::color`], which
    /// divides the wheel into six 43-wide sectors.
    pub const HUE_GREEN: u8 = 86;
    pub const HUE_BLUE: u8 = 172;

    /// Parameters Rain advertises.
    pub const RAIN_ONLY_COUNT: u8 = 5;

    /// The tuned defaults. Also the values advertised as each parameter's
    /// `default` to hosts.
    pub const DEFAULT: Self = Self {
        drops: 6,
        spawn_interval: 30,
        trail_len: 128,
        column_half_width: 14,
        rain_hue: Self::HUE_BLUE,
    };

    /// Number of parameters, i.e. the valid range of the indices accepted by
    /// [`Self::get`] and [`Self::set`].
    pub const COUNT: u8 = 5;

    /// Host-facing parameter names, in index order. `Spawn` carries its
    /// encoding in the name because the value is not milliseconds.
    pub const NAMES: [&'static str; Self::COUNT as usize] =
        ["Drops", "Spawn x10ms", "Trail", "Width", "Rain hue"];

    /// Inclusive lower bound of each parameter, in index order.
    pub const MINS: [u8; Self::COUNT as usize] = [
        Self::DROPS_MIN,
        Self::SPAWN_INTERVAL_MIN,
        Self::TRAIL_LEN_MIN,
        Self::COLUMN_HALF_WIDTH_MIN,
        Self::HUE_MIN,
    ];

    /// Inclusive upper bound of each parameter, in index order.
    pub const MAXES: [u8; Self::COUNT as usize] = [
        Self::DROPS_MAX,
        Self::SPAWN_INTERVAL_MAX,
        Self::TRAIL_LEN_MAX,
        Self::COLUMN_HALF_WIDTH_MAX,
        Self::HUE_MAX,
    ];

    /// Default of each parameter, in index order.
    pub const DEFAULTS: [u8; Self::COUNT as usize] = [
        Self::DEFAULT.drops,
        Self::DEFAULT.spawn_interval,
        Self::DEFAULT.trail_len,
        Self::DEFAULT.column_half_width,
        Self::DEFAULT.rain_hue,
    ];

    /// Value of one parameter, or `None` when `index` is out of range.
    pub const fn get(&self, index: u8) -> Option<u8> {
        Some(match index {
            0 => self.drops,
            1 => self.spawn_interval,
            2 => self.trail_len,
            3 => self.column_half_width,
            4 => self.rain_hue,
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
            4 => self.rain_hue = value,
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
    /// Per-drop fraction of the nominal fall rate, 255 being full speed.
    ///
    /// Without this every drop takes exactly the same time to cross, so a
    /// group born together dies together. Once the slots are saturated a
    /// spawn can only happen when one expires, and clustered expiries mean
    /// clustered spawns -- the cluster sustains itself and the board pulses
    /// at the fall period. Varied rates smear each group apart, and varied
    /// speeds read as more natural rain besides.
    rate: u8,
    active: bool,
}

/// Slowest a drop may fall, as a fraction of the nominal rate out of 255.
const RATE_MIN: u8 = 160;

/// Milliseconds a drop at `rate` takes to fall from above the board to past
/// the bottom. Inverts the head equation in the tick loop.
fn fall_duration_ms(speed: u8, overshoot: i32, rate: u8) -> u32 {
    let span = 2 * (255 + overshoot) as u32;
    let per_tick = (1 + speed as u32 / 4).max(1);
    span * 255 * 256 / (rate as u32 * per_tick).max(1)
}

pub struct RainState {
    drops: [Drop; RAIN_DROPS],
    next_spawn_ms: u32,
    /// The animation clock at the previous frame, so a clock that jumps
    /// backward can be told from one that simply advanced.
    last_timer_ms: u32,
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
                rate: 255,
                active: false,
            }; RAIN_DROPS],
            next_spawn_ms: 0,
            last_timer_ms: 0,
            initialized: false,
            params,
        }
    }

    /// Retune a running effect. Drops already in flight keep falling with
    /// the new trail and column geometry; lowering the drop count simply
    /// stops the surplus slots from respawning once they expire.
    pub const fn set_params(&mut self, params: RainParams) {
        self.params = params;
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
        state.last_timer_ms = spawn_ms;
        state.drops[0] = Drop {
            spawn_ms,
            x,
            rate: 255,
            active: true,
        };
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
        let mut rng = rng;
        let count = layout.count();
        let active_drops = self.params.active_drops();
        let trail_len = self.params.trail_len.max(1) as u16;
        let col_half_width = self.params.column_half_width.max(1) as u16;
        // A drop dies once its whole trail has cleared y=255. The head starts
        // at the top rather than a trail-length above it: leading in from
        // off-board held a slot for more than half the drop's life at long
        // trails, and a slot occupied by an invisible drop is one that cannot
        // be raining. Entering at y=0 just means the trail is clipped until
        // the head has fallen far enough to draw all of it.
        let overshoot = trail_len as i32;

        // Every drop time and the spawn deadline are readings of the animation
        // clock, so a clock that jumped backward leaves all of them describing
        // a timeline that no longer exists. Start over: re-priming costs one
        // frame and refills the board, where keeping the old state would strand
        // the rain until the clock had climbed all the way back.
        if self.initialized && super::clock_rewound(self.last_timer_ms, params.timer_ms) {
            self.initialized = false;
        }
        self.last_timer_ms = params.timer_ms;

        if !self.initialized {
            self.initialized = true;
            self.next_spawn_ms = params.timer_ms;

            // Fill the slots as though it had already been raining, spreading
            // the drops evenly down the fall instead of releasing them all
            // from the top. A cadence shorter than one fall divided by the
            // slot count cannot be sustained -- past that point a spawn has to
            // wait for an expiry -- so whatever pattern the expiries have is
            // the pattern the board keeps. Starting them bunched makes that
            // pattern a clump that never disperses, which is what pulsed.
            let spread = fall_duration_ms(params.speed, overshoot, RATE_MIN);
            for slot in 0..active_drops {
                let rate = RATE_MIN.saturating_add(rng() / 3);
                let age = spread * slot as u32 / active_drops as u32;
                self.drops[slot] = Drop {
                    spawn_ms: params.timer_ms.wrapping_sub(age),
                    x: rng(),
                    rate,
                    active: true,
                };
            }
        }

        // Spawn when the inter-drop timer has elapsed (wraparound-safe, as in
        // Ripple). A cadence shorter than the frame interval owes more than
        // one drop per tick, so keep spawning while the deadline is behind
        // rather than once per frame -- otherwise the frame rate, not the
        // cadence, would set the ceiling on density.
        let due = |deadline: u32| params.timer_ms.wrapping_sub(deadline) < u32::MAX / 2;

        // A deadline left far behind (the effect was just selected, or the
        // board slept) restarts from now rather than firing every slot at once.
        if due(self.next_spawn_ms)
            && params.timer_ms.wrapping_sub(self.next_spawn_ms) > CADENCE_RESYNC_MS
        {
            self.next_spawn_ms = params.timer_ms;
        }

        // Each pass fills one slot, so the slot count bounds the loop.
        for _ in 0..active_drops {
            if !due(self.next_spawn_ms) {
                break;
            }
            // Take any free slot, not the next one in sequence. Waiting on one
            // particular slot lets a drop still in flight stall spawning while
            // the deadline keeps slipping, and the debt is then repaid all at
            // once the moment that slot expires -- drops arrive in clumps that
            // share a spawn time, fall as a rigid group, and expire together,
            // which reads as a periodic burst rather than steady rain.
            let Some(slot) = self.drops[..active_drops].iter().position(|d| !d.active) else {
                // Genuinely saturated: hold the cadence at the current time
                // instead of banking a backlog to dump later.
                self.next_spawn_ms = params
                    .timer_ms
                    .wrapping_add(self.params.spawn_delay_ms(rng()).max(1));
                break;
            };
            self.drops[slot] = Drop {
                spawn_ms: params.timer_ms,
                x: rng(),
                rate: RATE_MIN.saturating_add(rng() / 3),
                active: true,
            };
            // Advance from the deadline, not from now, so a cadence finer than
            // the frame interval keeps its average rate.
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
            let nominal = scale16by8(elapsed as u16, 1 + params.speed / 4) as i32;
            let tick = nominal * drop.rate as i32 / 255;
            let y = tick / 2;
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

            // Brightness follows intensity, so the background stays black and
            // a trail dims toward its tail. Hue and saturation are the
            // effect's own rather than the palette's: palettes desaturate
            // toward their bright end, which would render exactly the parts
            // that matter -- drop heads and key hits -- white, hiding the
            // chosen hues.
            *slot = Hsv {
                h: self.params.rain_hue,
                s: params.sat,
                v: scale8(intensity, params.val),
            };
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

    /// A Glove80-shaped grid: 20 columns of 6 rows on the 0..=255 grid.
    fn board_positions() -> [(u8, u8); 80] {
        let mut positions = [(0u8, 0u8); 80];
        for (i, p) in positions.iter_mut().enumerate() {
            let col = (i % 20) as u32;
            let row = (i / 20) as u32;
            *p = ((col * 255 / 19) as u8, (row * 255 / 5) as u8);
        }
        positions
    }

    /// A dense, board-filling tuning, as the effect is actually run.
    fn dense() -> RainParams {
        RainParams {
            drops: 28,
            spawn_interval: 5,
            trail_len: 161,
            ..RainParams::DEFAULT
        }
    }

    fn walking_entropy() -> impl FnMut() -> u8 {
        let mut entropy = 7u8;
        move || {
            entropy = entropy.wrapping_mul(37).wrapping_add(11);
            entropy
        }
    }

    #[test]
    fn drop_sweeps_down_its_column_with_a_trail() {
        let layout = SliceLayout::new(COLUMN);
        // One drop, so the sweep is this drop's and not the primed population's.
        let mut state = RainState::with_single_drop(0, 0);
        let mut out = [Hsv::default(); 5];

        // The head enters at the top, so the first row lights straight away
        // while the rest of the column is still dark.
        state.tick(&layout, params(0, 128), || 0, &mut out);
        assert!(out[0].v > 0, "the head should enter at the top row");
        assert_eq!(lit(&out), 1);

        // Mid-fall the head is further down and a trail follows it. The window
        // covers the slowest fall rate a drop can draw, not just the nominal.
        let mut saw_partial = false;
        for t in (100..8000).step_by(100) {
            state.tick(&layout, params(t, 128), || 0, &mut out);
            let n = lit(&out);
            if n > 1 && n < COLUMN.len() {
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
                params.column_half_width,
                params.rain_hue
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

    /// Rain has to arrive steadily, not in waves. Measured across a whole
    /// board at the dense setting, because the failure is a population that
    /// collapses and refills rather than anything visible on one LED.
    ///
    /// Beyond about one fall per slot the cadence cannot be sustained: a spawn
    /// has to wait for an expiry, so the expiry pattern becomes the spawn
    /// pattern and whatever clumping exists sustains itself. Three things keep
    /// it dispersed -- the first fill is spread down the fall rather than
    /// released from the top, drops fall at slightly different rates so a
    /// group cannot expire together, and a drop no longer holds a slot while
    /// it is still off-board above the top row.
    #[test]
    fn dense_rain_holds_a_steady_population_across_the_board() {
        let positions = board_positions();
        let layout = SliceLayout::new(&positions);
        let mut out = [Hsv::default(); 80];

        let mut state = RainState::with_params(dense());
        let mut rng = walking_entropy();

        let (mut lowest, mut highest) = (usize::MAX, 0usize);
        for step in 0..200u32 {
            state.tick(
                &layout,
                FrameParams {
                    palette: &CARNIVAL,
                    speed: 221,
                    sat: 255,
                    val: 255,
                    timer_ms: step * 100,
                },
                &mut rng,
                &mut out,
            );
            // Skip the first couple of seconds so this measures the sustained
            // pattern, not the ramp.
            if step >= 20 {
                let lit = out.iter().filter(|h| h.v > 0).count();
                lowest = lowest.min(lit);
                highest = highest.max(lit);
            }
        }

        // Releasing every drop from the top swung between 4 and 62 lit LEDs;
        // spreading the first fill alone still bottomed out near 24. Steady
        // rain keeps the trough within a factor of two of the peak.
        assert!(
            lowest * 2 > highest,
            "population swung between {lowest} and {highest} lit LEDs"
        );
    }

    /// The animation clock is not guaranteed to move forward. A split
    /// peripheral renders on the central's clock, so its time snaps back to
    /// the central's uptime every time the central reboots while the
    /// peripheral keeps running -- reflashing or replugging one half is
    /// enough. A spawn only ever pushes the deadline forward, so before this
    /// was handled the rain stopped for as wide as the jump: on the board
    /// that found it, the right half went hours with no drops at all while
    /// reactive key hits carried on working, because those are stamped when
    /// they happen rather than read off a deadline.
    #[test]
    fn rain_comes_back_after_the_clock_jumps_backward() {
        let positions = board_positions();
        let layout = SliceLayout::new(&positions);
        let mut out = [Hsv::default(); 80];

        let mut state = RainState::with_params(dense());
        let mut rng = walking_entropy();

        // An hour of central uptime, replicated to the peripheral.
        const UPTIME_MS: u32 = 60 * 60 * 1000;
        for step in 0..50u32 {
            state.tick(
                &layout,
                params(UPTIME_MS + step * 40, 128),
                &mut rng,
                &mut out,
            );
        }
        assert!(lit(&out) > 0, "the board should be raining before the jump");

        // The central reboots; the shared clock restarts from its uptime.
        let mut peak = 0;
        for step in 0..50u32 {
            state.tick(&layout, params(step * 40, 128), &mut rng, &mut out);
            peak = peak.max(lit(&out));
        }
        assert!(
            peak > 0,
            "the rain never returned after the clock moved back"
        );
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
