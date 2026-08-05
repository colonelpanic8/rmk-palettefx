//! COMET: a body that flies to the keys you press, arriving late, carrying its
//! momentum through the turn, and dragging a short trail behind it.
//!
//! Every other typing-reactive effect draws where a key *was* pressed. Comet
//! draws one object that still has to get there, so a burst of typing reads as
//! a flight path rather than as a set of independent flashes.
//!
//! The flight is a chain of legs. A press ends the leg in progress and opens a
//! new one starting from the position and velocity the comet had at that
//! instant, so a redirect mid-flight bends the path and swings wide of the new
//! key instead of snapping onto the new heading.
//!
//! Each leg is a closed form of its own start state and elapsed time, never an
//! integration of the previous frame. That is not a stylistic choice: the two
//! halves of a split keyboard render this effect independently against a
//! shared clock and on their own frame cadences, so anything carried frame to
//! frame would drift the two bodies apart and tear the comet in half as it
//! crossed the seam. Replaying a leg from its recorded start makes both halves
//! agree by construction.

use super::{FrameParams, clock_rewound};
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::palette::interp_color;

/// Sub-unit resolution of every position in this module: board coordinates
/// (0..=255 on both axes) scaled by 16. Whole board units are far too coarse
/// for a body in flight -- a comet crossing the board in a quarter second
/// moves several of them per frame, and rounding to them makes it stutter.
const POS_ONE: i32 = 16;

/// Fixed-point one for the Hermite curve parameter and its bases.
const CURVE_ONE: i32 = 1024;

/// Velocity ceiling in board units per second. Nothing a keyboard produces
/// approaches it; it is here so that a pathological chain of redirects cannot
/// overflow the leg arithmetic or the `i16` a leg stores its velocity in.
const VEL_LIMIT: i32 = 8_000;

/// Positions sampled behind the head. The trail is the polyline through them,
/// so this is a smoothness budget rather than a length -- how far back the
/// last one reaches is [`CometParams::trail`].
const TRAIL_SAMPLES: usize = 8;

/// Brightness of the trail where it leaves the head, out of 255.
///
/// Deliberately equal to [`CORE_THRESHOLD`]: the trail is then exactly as
/// bright as the head's outer edge, so the two meet without a seam and no
/// trail pixel ever reaches the desaturated core.
const TRAIL_PEAK: u8 = 190;

/// Palette rotation per trail sample. Small, so a trail laid down within one
/// leg still shades along its length.
const TRAIL_HUE_STEP: u8 = 3;

/// Coverage above which a pixel washes out toward white, and how many points
/// of saturation each point of coverage past it costs. The hot core is what
/// makes the head read as a body with a tail rather than as a bright smudge
/// travelling alongside one.
const CORE_THRESHOLD: u8 = 190;
const CORE_DESATURATION: u16 = 2;

/// A leg whose start is further ahead of a sample time than this did not start
/// in the future, it wrapped the millisecond counter.
const FUTURE_GUARD: u32 = u32::MAX / 2;

/// Distance, in board units, that [`CometParams::lag`] times: a flight from
/// one edge of the board to the other.
const BOARD_SPAN: u32 = 255;

/// Longest hop the board can ask for, its diagonal. Caps the distance a
/// travel time is derived from so the arithmetic stays bounded.
const BOARD_DIAGONAL: u32 = 361;

/// Floor on a leg's travel time. Below roughly one frame the comet would jump
/// rather than fly, however short the hop is.
const MIN_TRAVEL_MS: u32 = 40;

/// Runtime controls for [`CometState`].
///
/// Every field is a byte so it can be exposed directly through a firmware
/// parameter protocol. Values changed through [`Self::set`] are range checked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CometParams {
    /// Time for a flight the full width of the board, in units of 10
    /// milliseconds. This is the lag at its worst: how far behind your typing
    /// the comet runs when a press sends it all the way across. The shared
    /// speed control scales it, with `128` preserving the configured value.
    pub lag: u8,
    /// How strongly the distance of a hop sets its travel time.
    ///
    /// `0` gives every flight the same time whatever the distance, so a hop to
    /// the next key over crawls while one across the board tears across it,
    /// and a burst of typing leaves the comet perpetually mid-flight toward a
    /// target that has already moved. `255` makes the time follow the square
    /// root of the distance instead: short hops finish quickly, long ones stay
    /// bounded by `lag`, and the comet catches up between keystrokes rather
    /// than falling further behind with every one.
    pub pace: u8,
    /// Fraction of the in-flight velocity a redirect inherits, out of 128.
    /// `0` stops the comet dead at every press and `128` conserves the
    /// velocity exactly; above that it gains speed through a turn and swings
    /// wider, which is the setting that reads as weight.
    pub momentum: u8,
    /// How far back along the flown path the trail reaches, in units of 10
    /// milliseconds. `0` leaves a bare head.
    pub trail: u8,
    /// Radius of the head in board units.
    pub head: u8,
    /// How long the comet takes to fade out once it has arrived and no
    /// further key has been pressed, in units of 10 milliseconds.
    pub linger: u8,
}

impl CometParams {
    pub const LAG_MIN: u8 = 4;
    pub const LAG_MAX: u8 = 120;
    pub const PACE_MIN: u8 = 0;
    pub const PACE_MAX: u8 = 255;
    pub const MOMENTUM_MIN: u8 = 0;
    pub const MOMENTUM_MAX: u8 = 255;
    pub const TRAIL_MIN: u8 = 0;
    pub const TRAIL_MAX: u8 = 80;
    pub const HEAD_MIN: u8 = 4;
    pub const HEAD_MAX: u8 = 40;
    pub const LINGER_MIN: u8 = 4;
    pub const LINGER_MAX: u8 = 255;

    /// Velocity is conserved exactly at this value; [`Self::momentum`] is a
    /// fraction of it.
    pub const MOMENTUM_UNIT: u8 = 128;

    /// The tuned defaults: a third of a second to cross the whole board, most
    /// of that time budget released for shorter hops, enough momentum to
    /// overshoot visibly, and a trail about as long as one flight.
    pub const DEFAULT: Self = Self {
        lag: 32,
        pace: 200,
        momentum: 168,
        trail: 20,
        head: 14,
        linger: 80,
    };

    pub const COUNT: u8 = 6;

    pub const NAMES: [&'static str; Self::COUNT as usize] = [
        "Lag x10ms",
        "Distance pace",
        "Momentum",
        "Trail x10ms",
        "Head size",
        "Linger x10ms",
    ];

    pub const MINS: [u8; Self::COUNT as usize] = [
        Self::LAG_MIN,
        Self::PACE_MIN,
        Self::MOMENTUM_MIN,
        Self::TRAIL_MIN,
        Self::HEAD_MIN,
        Self::LINGER_MIN,
    ];

    pub const MAXES: [u8; Self::COUNT as usize] = [
        Self::LAG_MAX,
        Self::PACE_MAX,
        Self::MOMENTUM_MAX,
        Self::TRAIL_MAX,
        Self::HEAD_MAX,
        Self::LINGER_MAX,
    ];

    pub const DEFAULTS: [u8; Self::COUNT as usize] = [
        Self::DEFAULT.lag,
        Self::DEFAULT.pace,
        Self::DEFAULT.momentum,
        Self::DEFAULT.trail,
        Self::DEFAULT.head,
        Self::DEFAULT.linger,
    ];

    /// Value of one parameter, or `None` for an unknown index.
    pub const fn get(&self, index: u8) -> Option<u8> {
        Some(match index {
            0 => self.lag,
            1 => self.pace,
            2 => self.momentum,
            3 => self.trail,
            4 => self.head,
            5 => self.linger,
            _ => return None,
        })
    }

    /// Set one parameter, leaving `self` unchanged if index or value is invalid.
    pub const fn set(&mut self, index: u8, value: u8) -> bool {
        let i = index as usize;
        if i >= Self::COUNT as usize || value < Self::MINS[i] || value > Self::MAXES[i] {
            return false;
        }
        match index {
            0 => self.lag = value,
            1 => self.pace = value,
            2 => self.momentum = value,
            3 => self.trail = value,
            4 => self.head = value,
            5 => self.linger = value,
            _ => return false,
        }
        true
    }
}

impl Default for CometParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One flight from wherever the comet was to the key that interrupted it.
///
/// The start position and velocity are resolved when the press is recorded, so
/// replaying the leg needs nothing but the leg itself and a time.
#[derive(Copy, Clone)]
struct Leg {
    spawn_ms: u32,
    /// Where the comet was when this leg opened, `POS_ONE`-scaled.
    from_x: i16,
    from_y: i16,
    /// Velocity carried into this leg, board units per second.
    vel_x: i16,
    vel_y: i16,
    /// Travel time, resolved at the press from this hop's distance and the
    /// tuning and speed in force then. Held per leg both because it varies
    /// with distance and so that retuning paces the next press rather than
    /// warping the arc of one already flying.
    duration_ms: u16,
    to_x: u8,
    to_y: u8,
    palette_pos: u8,
    /// Whether this leg began a new flight rather than redirecting the one in
    /// progress. The trail is not allowed to reach back past it: the comet was
    /// not anywhere before this press, and drawing through where the previous
    /// flight died would stretch a stroke across the whole board.
    restart: bool,
}

const EMPTY_LEG: Leg = Leg {
    spawn_ms: 0,
    from_x: 0,
    from_y: 0,
    vel_x: 0,
    vel_y: 0,
    duration_ms: 1,
    to_x: 0,
    to_y: 0,
    palette_pos: 0,
    restart: false,
};

/// A point on the flown path, with the color the comet wore there.
#[derive(Copy, Clone)]
struct Trace {
    x: i32,
    y: i32,
    palette_pos: u8,
}

const EMPTY_TRACE: Trace = Trace {
    x: 0,
    y: 0,
    palette_pos: 0,
};

/// Ring of the `N` most recent flight legs and the current tuning.
///
/// `N` bounds how far back the trail can be reconstructed: samples older than
/// the oldest remembered leg fall back to where that leg started.
pub struct CometState<const N: usize> {
    legs: [Leg; N],
    /// Legs written so far, saturating at `N`.
    filled: usize,
    next: usize,
    color_step: u8,
    /// Frame speed seen on the last rendered frame. A press is recorded
    /// outside any frame, and a leg's travel time has to be fixed when it
    /// opens for its start velocity to describe the same curve the renderer
    /// draws.
    last_speed: u8,
    last_timer_ms: u32,
    params: CometParams,
}

impl<const N: usize> Default for CometState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> CometState<N> {
    pub const fn new() -> Self {
        Self::with_params(CometParams::DEFAULT)
    }

    /// State pre-tuned with `params`; see [`CometState::set_params`] for
    /// changing them on a running effect.
    pub const fn with_params(params: CometParams) -> Self {
        Self {
            legs: [EMPTY_LEG; N],
            filled: 0,
            next: 0,
            color_step: 0,
            last_speed: 128,
            last_timer_ms: 0,
            params,
        }
    }

    /// Retune a running effect. The leg in flight keeps the travel time it
    /// launched with and finishes the arc it is on.
    pub const fn set_params(&mut self, params: CometParams) {
        self.params = params;
    }

    /// The tuning currently in effect.
    pub const fn params(&self) -> CometParams {
        self.params
    }

    pub fn record_hit(&mut self, x: u8, y: u8, timer_ms: u32) {
        if N == 0 {
            return;
        }
        if clock_rewound(self.last_timer_ms, timer_ms) {
            self.clear();
        }
        self.last_timer_ms = timer_ms;

        // A press that lands while the comet is still lit hands it its current
        // position and velocity. One that lands after it has faded out starts
        // a fresh flight at the key instead of dragging the body in from
        // wherever it happened to die, which would be a long unrelated swoop.
        let flying = self.flight_state(timer_ms);
        let (from_x, from_y, vel_x, vel_y) = match flying {
            Some((px, py, vx, vy)) => (px, py, self.carry(vx), self.carry(vy)),
            None => (x as i32 * POS_ONE, y as i32 * POS_ONE, 0, 0),
        };
        self.legs[self.next] = Leg {
            spawn_ms: timer_ms,
            from_x: clamp_pos(from_x),
            from_y: clamp_pos(from_y),
            vel_x,
            vel_y,
            duration_ms: self.travel_ms(hop_distance(from_x, from_y, x, y)) as u16,
            to_x: x,
            to_y: y,
            palette_pos: x.wrapping_add(y / 2).wrapping_add(self.color_step),
            restart: flying.is_none(),
        };
        self.next = (self.next + 1) % N;
        self.filled = (self.filled + 1).min(N);
        self.color_step = self.color_step.wrapping_add(29);
    }

    pub fn tick<L: LedLayout>(&mut self, layout: &L, frame: FrameParams<'_>, out: &mut [Hsv]) {
        self.render(layout, frame, out);
    }

    pub fn tick_layer<L: LedLayout>(
        &mut self,
        layout: &L,
        frame: FrameParams<'_>,
        out: &mut [Hsv],
    ) {
        self.render(layout, frame, out);
    }

    fn render<L: LedLayout>(&mut self, layout: &L, frame: FrameParams<'_>, out: &mut [Hsv]) {
        out.fill(Hsv::default());
        // Every leg is stamped against the animation clock, so a clock that
        // jumped backward leaves the whole flight describing a timeline that
        // no longer exists. Drop it; the next press starts a new one.
        if clock_rewound(self.last_timer_ms, frame.timer_ms) {
            self.clear();
        }
        self.last_timer_ms = frame.timer_ms;
        self.last_speed = frame.speed;

        let Some((leg, elapsed)) = self.leg_at(frame.timer_ms) else {
            return;
        };
        let brightness = self.envelope(leg, elapsed);
        if brightness == 0 {
            return;
        }

        let (head_x, head_y) = leg_position(leg, elapsed);
        let mut path = [EMPTY_TRACE; TRAIL_SAMPLES + 1];
        path[0] = Trace {
            x: head_x,
            y: head_y,
            palette_pos: leg.palette_pos,
        };
        let mut points = 1;
        let step = self.trail_ms() / TRAIL_SAMPLES as u32;
        let launched = self.flight_start_ms(frame.timer_ms);
        if step != 0 {
            for (behind, slot) in path.iter_mut().skip(1).enumerate() {
                let sample = behind as u32 + 1;
                let at = frame.timer_ms.wrapping_sub(step * sample);
                if launched.is_some_and(|start| at.wrapping_sub(start) > FUTURE_GUARD) {
                    break;
                }
                let Some(mut trace) = self.trace_at(at) else {
                    break;
                };
                trace.palette_pos = trace
                    .palette_pos
                    .wrapping_add(sample as u8 * TRAIL_HUE_STEP);
                *slot = trace;
                points += 1;
            }
        }

        let head = self.params.head;
        // Half the head, so the trail leaves the body already narrower than it
        // rather than bulging out of its sides.
        let tail = (head / 2).max(1);

        for (led_idx, pixel) in out.iter_mut().take(layout.count()).enumerate() {
            let (led_x, led_y) = layout.position(led_idx);
            let (px, py) = (led_x as i32 * POS_ONE, led_y as i32 * POS_ONE);
            let mut strongest = point_coverage(px, py, path[0].x, path[0].y, head);
            let mut palette_pos = path[0].palette_pos;
            for (stroke, ends) in path[..points].windows(2).enumerate() {
                let coverage = scale255(
                    segment_coverage(px, py, ends[0].x, ends[0].y, ends[1].x, ends[1].y, tail),
                    trail_fade(stroke + 1),
                );
                if coverage > strongest {
                    strongest = coverage;
                    palette_pos = ends[1].palette_pos;
                }
            }
            if strongest == 0 {
                continue;
            }
            *pixel = interp_color(
                frame.palette,
                palette_pos,
                core_sat(frame.sat, strongest),
                scale255(scale255(strongest, brightness), frame.val),
            );
        }
    }

    fn clear(&mut self) {
        self.filled = 0;
        self.next = 0;
    }

    /// The velocity a redirect inherits from `velocity`.
    fn carry(&self, velocity: i32) -> i16 {
        let scaled = velocity * self.params.momentum as i32 / CometParams::MOMENTUM_UNIT as i32;
        scaled.clamp(-VEL_LIMIT, VEL_LIMIT) as i16
    }

    /// Position (`POS_ONE`-scaled) and velocity (board units per second) of a
    /// comet still in flight, or `None` once it has faded out.
    fn flight_state(&self, timer_ms: u32) -> Option<(i32, i32, i32, i32)> {
        let (leg, elapsed) = self.leg_at(timer_ms)?;
        if self.envelope(leg, elapsed) == 0 {
            return None;
        }
        let (x, y) = leg_position(leg, elapsed);
        let (vx, vy) = leg_velocity(leg, elapsed);
        Some((x, y, vx, vy))
    }

    /// The remembered legs, newest first.
    ///
    /// A frame can drain several presses at once and stamps them all with the
    /// same time, so "newest" has to mean the order they were recorded in and
    /// not the lowest elapsed time. Walking the ring backward from the write
    /// cursor is that order.
    fn legs_newest_first(&self) -> impl Iterator<Item = Leg> + '_ {
        (0..self.filled).map(move |back| self.legs[(self.next + N - 1 - back) % N])
    }

    /// The most recently opened leg that had already started at `timer_ms`,
    /// with how long it had been running by then.
    fn leg_at(&self, timer_ms: u32) -> Option<(Leg, u32)> {
        let mut current: Option<(Leg, u32)> = None;
        for leg in self.legs_newest_first() {
            let elapsed = timer_ms.wrapping_sub(leg.spawn_ms);
            if elapsed > FUTURE_GUARD {
                continue;
            }
            let closer = match current {
                Some((_, best)) => elapsed < best,
                None => true,
            };
            if closer {
                current = Some((leg, elapsed));
            }
        }
        current
    }

    /// When the flight in progress at `timer_ms` began: the most recent press
    /// that started the comet over rather than redirecting it.
    fn flight_start_ms(&self, timer_ms: u32) -> Option<u32> {
        self.legs_newest_first()
            .find(|leg| timer_ms.wrapping_sub(leg.spawn_ms) <= FUTURE_GUARD && leg.restart)
            .map(|leg| leg.spawn_ms)
    }

    /// The leg that opens soonest after `timer_ms`, and the first of those if
    /// several open together -- its start is where the comet actually was.
    fn leg_after(&self, timer_ms: u32) -> Option<Leg> {
        let mut upcoming: Option<(Leg, u32)> = None;
        for leg in self.legs_newest_first() {
            let ahead = leg.spawn_ms.wrapping_sub(timer_ms);
            if ahead == 0 || ahead > FUTURE_GUARD {
                continue;
            }
            let closer = match upcoming {
                Some((_, best)) => ahead <= best,
                None => true,
            };
            if closer {
                upcoming = Some((leg, ahead));
            }
        }
        upcoming.map(|(leg, _)| leg)
    }

    /// Where the comet was at `timer_ms`, and what color it wore there.
    ///
    /// Once the ring has wrapped, the oldest trail samples reach back past
    /// every leg still remembered. The comet was sitting still at the start of
    /// the oldest one then, so that is what those samples get -- the trail
    /// bunches up at its tip rather than jumping to an unrelated position.
    fn trace_at(&self, timer_ms: u32) -> Option<Trace> {
        if let Some((leg, elapsed)) = self.leg_at(timer_ms) {
            let (x, y) = leg_position(leg, elapsed);
            return Some(Trace {
                x,
                y,
                palette_pos: leg.palette_pos,
            });
        }
        let leg = self.leg_after(timer_ms)?;
        Some(Trace {
            x: leg.from_x as i32,
            y: leg.from_y as i32,
            palette_pos: leg.palette_pos,
        })
    }

    /// Brightness of the whole comet `elapsed_ms` into `leg`. It holds full
    /// brightness for as long as it is still travelling, then fades over the
    /// linger time.
    fn envelope(&self, leg: Leg, elapsed_ms: u32) -> u8 {
        let Some(fading) = elapsed_ms.checked_sub(leg.duration_ms as u32) else {
            return 255;
        };
        let linger = self.linger_ms();
        if fading >= linger {
            return 0;
        }
        (255 - fading * 255 / linger) as u8
    }

    /// Travel time for a hop of `distance` board units.
    ///
    /// [`CometParams::lag`] buys a flight across the whole board;
    /// [`CometParams::pace`] decides how much of that budget a shorter hop
    /// gives back. Giving it back is what keeps the comet in touch with fast
    /// typing: each press restarts the flight, so if every hop cost a full lag
    /// the comet would spend a burst of typing drifting toward a target that
    /// moved again before it arrived, and the further behind it fell the
    /// longer it would stay there.
    fn travel_ms(&self, distance: u32) -> u32 {
        let base = scaled_ms(self.params.lag, self.last_speed);
        // `base * sqrt(distance / BOARD_SPAN)`, kept in integers. The square
        // root rather than the distance itself: a hop twice as long should
        // take longer, but not twice as long, or the comet's speed swings by
        // the same factor the board is wide.
        let by_distance = base * (distance * BOARD_SPAN).isqrt() / BOARD_SPAN;
        lerp(base, by_distance, self.params.pace).clamp(MIN_TRAVEL_MS, u16::MAX as u32)
    }

    fn linger_ms(&self) -> u32 {
        scaled_ms(self.params.linger, self.last_speed)
    }

    fn trail_ms(&self) -> u32 {
        self.params.trail as u32 * 10 * (384 - self.last_speed as u32) / 256
    }
}

/// A duration parameter in units of 10 milliseconds, scaled by the shared
/// speed control the same way every other effect's lifetime is.
fn scaled_ms(units: u8, speed: u8) -> u32 {
    (units as u32 * 10 * (384 - speed as u32) / 256).max(1)
}

/// Straight-line distance in board units from a `POS_ONE`-scaled position to
/// a key, capped at the longest hop the board can ask for.
fn hop_distance(from_x: i32, from_y: i32, to_x: u8, to_y: u8) -> u32 {
    let dx = ((to_x as i32 * POS_ONE - from_x) / POS_ONE).unsigned_abs();
    let dy = ((to_y as i32 * POS_ONE - from_y) / POS_ONE).unsigned_abs();
    (dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)))
        .isqrt()
        .min(BOARD_DIAGONAL)
}

/// Linear interpolation between two durations, `frac` out of 255.
fn lerp(a: u32, b: u32, frac: u8) -> u32 {
    if b >= a {
        a + (b - a) * frac as u32 / 255
    } else {
        a - (a - b) * frac as u32 / 255
    }
}

/// Brightness of the trail stroke ending at sample `index`, out of 255.
fn trail_fade(index: usize) -> u8 {
    let remaining = (TRAIL_SAMPLES + 1 - index) as u32;
    (TRAIL_PEAK as u32 * remaining / (TRAIL_SAMPLES + 1) as u32) as u8
}

/// Saturation for a pixel with `coverage`, washing the hot core toward white.
fn core_sat(sat: u8, coverage: u8) -> u8 {
    let Some(excess) = coverage.checked_sub(CORE_THRESHOLD) else {
        return sat;
    };
    sat - (excess as u16 * CORE_DESATURATION).min(sat as u16) as u8
}

fn clamp_pos(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Where `leg` has the comet `elapsed_ms` in, `POS_ONE`-scaled.
fn leg_position(leg: Leg, elapsed_ms: u32) -> (i32, i32) {
    let u = curve_param(elapsed_ms, leg.duration_ms);
    (
        hermite(leg.from_x, leg.to_x, leg.vel_x, leg.duration_ms, u),
        hermite(leg.from_y, leg.to_y, leg.vel_y, leg.duration_ms, u),
    )
}

/// How fast `leg` has the comet moving `elapsed_ms` in, in board units per
/// second.
fn leg_velocity(leg: Leg, elapsed_ms: u32) -> (i32, i32) {
    let u = curve_param(elapsed_ms, leg.duration_ms);
    (
        hermite_slope(leg.from_x, leg.to_x, leg.vel_x, leg.duration_ms, u),
        hermite_slope(leg.from_y, leg.to_y, leg.vel_y, leg.duration_ms, u),
    )
}

/// Progress through a leg as `CURVE_ONE`-scaled 0..=1, held at the end once
/// the leg has been completed.
fn curve_param(elapsed_ms: u32, duration_ms: u16) -> i32 {
    let duration = duration_ms.max(1) as u32;
    (elapsed_ms.min(duration) * CURVE_ONE as u32 / duration) as i32
}

/// One axis of a cubic Hermite from `from` at the carried velocity to `to` at
/// rest, `POS_ONE`-scaled.
///
/// The carried velocity is what gives the flight its weight: redirecting a
/// comet that is still moving fast swings it wide of the new key and lets it
/// drift back, where a plain ease would bend it onto the new heading at once.
fn hermite(from: i16, to: u8, vel: i16, duration_ms: u16, u: i32) -> i32 {
    let to = to as i32 * POS_ONE;
    let u2 = u * u / CURVE_ONE;
    let u3 = u2 * u / CURVE_ONE;
    // `2u^3 - 3u^2 + 1`: one at the start, zero at the end, flat at both.
    let start = 2 * u3 - 3 * u2 + CURVE_ONE;
    // `u^3 - 2u^2 + u`: zero at both ends, unit slope at the start.
    let carry = u3 - 2 * u2 + u;
    to + (from as i32 - to) * start / CURVE_ONE + leg_reach(vel, duration_ms) * carry / CURVE_ONE
}

/// The derivative of [`hermite`], converted from distance per leg into board
/// units per second.
fn hermite_slope(from: i16, to: u8, vel: i16, duration_ms: u16, u: i32) -> i32 {
    let to = to as i32 * POS_ONE;
    let u2 = u * u / CURVE_ONE;
    let start = 6 * u2 - 6 * u;
    let carry = 3 * u2 - 4 * u + CURVE_ONE;
    let slope =
        (from as i32 - to) * start / CURVE_ONE + leg_reach(vel, duration_ms) * carry / CURVE_ONE;
    slope * 1000 / (POS_ONE * duration_ms.max(1) as i32)
}

/// How far `vel` alone would carry the comet over a whole leg of
/// `duration_ms`, `POS_ONE`-scaled. This is the unit the Hermite velocity
/// basis is expressed in.
fn leg_reach(vel: i16, duration_ms: u16) -> i32 {
    vel as i32 * duration_ms as i32 * POS_ONE / 1000
}

/// Coverage of the LED at `(px, py)` by a round glow of `radius` board units
/// centred on `(cx, cy)`. Every coordinate is `POS_ONE`-scaled.
fn point_coverage(px: i32, py: i32, cx: i32, cy: i32, radius: u8) -> u8 {
    let limit = radius as u32 * POS_ONE as u32;
    let dx = (px - cx).unsigned_abs();
    let dy = (py - cy).unsigned_abs();
    if dx > limit || dy > limit {
        return 0;
    }
    soft_edge((dx * dx + dy * dy).isqrt(), limit)
}

/// Coverage of the LED at `(px, py)` by a stroke of half-width `width` board
/// units running from `a` to `b`. Past either end the stroke is capped with
/// the same round glow the head uses, so the joints between the trail's
/// segments do not show as notches.
fn segment_coverage(px: i32, py: i32, ax: i32, ay: i32, bx: i32, by: i32, width: u8) -> u8 {
    let limit = width as i32 * POS_ONE;
    if px < ax.min(bx) - limit
        || px > ax.max(bx) + limit
        || py < ay.min(by) - limit
        || py > ay.max(by) + limit
    {
        return 0;
    }

    let (vx, vy) = (bx - ax, by - ay);
    let len_sq = vx * vx + vy * vy;
    if len_sq == 0 {
        return point_coverage(px, py, ax, ay, width);
    }
    let (wx, wy) = (px - ax, py - ay);
    let dot = wx * vx + wy * vy;
    if dot <= 0 {
        return point_coverage(px, py, ax, ay, width);
    }
    if dot >= len_sq {
        return point_coverage(px, py, bx, by, width);
    }
    let length = (len_sq as u32).isqrt().max(1);
    let cross = (wx * vy - wy * vx).unsigned_abs();
    soft_edge(cross / length, limit as u32)
}

fn soft_edge(distance: u32, radius: u32) -> u8 {
    if distance > radius {
        0
    } else {
        (255 - distance * 255 / (radius + 1)) as u8
    }
}

const fn scale255(value: u8, scale: u8) -> u8 {
    ((value as u32 * scale as u32 + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;
    use crate::palette::CARNIVAL;

    /// A row across the whole board. Index 3 sits just past index 2, close
    /// enough that only an overshoot reaches it.
    const ROW: &[(u8, u8)] = &[
        (0, 128),
        (64, 128),
        (128, 128),
        (160, 128),
        (192, 128),
        (255, 128),
    ];

    fn frame(timer_ms: u32) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed: 128,
            sat: 255,
            val: 255,
            timer_ms,
        }
    }

    /// Time a flight across the whole board takes at the speed the test frames
    /// use, which is the neutral one. Shorter hops take less.
    fn full_board_travel() -> u32 {
        scaled_ms(CometParams::DEFAULT.lag, 128)
    }

    fn tuned<const N: usize>(params: CometParams) -> CometState<N> {
        CometState::with_params(params)
    }

    /// The comet does not teleport: pressed a board's width away from where it
    /// is, it is still back at the old key on the frame of the press and only
    /// reaches the new one a lag later.
    #[test]
    fn the_comet_arrives_a_lag_after_the_key_it_chases() {
        let layout = SliceLayout::new(ROW);
        let mut state = CometState::<8>::new();
        let mut out = [Hsv::default(); 6];

        state.record_hit(0, 128, 0);
        state.record_hit(255, 128, 0);

        state.tick(&layout, frame(0), &mut out);
        assert_ne!(out[0].v, 0, "the comet left before it was launched");
        assert_eq!(out[5].v, 0, "the comet reached the far key instantly");

        state.tick(&layout, frame(full_board_travel()), &mut out);
        assert_ne!(out[5].v, 0, "the comet never arrived");
    }

    /// A hop to a nearby key must not cost the same flight time as one across
    /// the board. Every press restarts the flight, so a comet that always
    /// spent a full lag would never finish one during a burst of typing -- it
    /// would drift after a target that keeps moving and fall further behind
    /// with every keystroke instead of catching up between them.
    #[test]
    fn a_short_hop_lands_while_a_full_board_flight_is_still_travelling() {
        let layout = SliceLayout::new(ROW);
        let sample = full_board_travel() * 3 / 4;

        let mut near = CometState::<8>::new();
        let mut out = [Hsv::default(); 6];
        near.record_hit(128, 128, 0);
        near.record_hit(192, 128, 0);
        near.tick(&layout, frame(sample), &mut out);
        assert_ne!(out[4].v, 0, "a short hop was still under way");

        let mut far = CometState::<8>::new();
        far.record_hit(0, 128, 0);
        far.record_hit(255, 128, 0);
        far.tick(&layout, frame(sample), &mut out);
        assert_eq!(out[5].v, 0, "a full-board flight arrived early");
    }

    /// Turning the pacing off restores one flight time for every distance,
    /// which is what a board that wants a constant lag asks for. Early in the
    /// same short hop the unpaced comet has barely left the key it set off
    /// from, where the fully paced one is already well clear of it.
    #[test]
    fn pace_decides_whether_a_short_hop_gets_a_short_flight() {
        let layout = SliceLayout::new(ROW);
        let early = full_board_travel() / 4;
        let still_at_origin = |pace| {
            // No trail: it runs back to the key the hop set off from, which is
            // the very LED this reads.
            let mut state = tuned::<8>(CometParams {
                pace,
                trail: 0,
                ..CometParams::DEFAULT
            });
            let mut out = [Hsv::default(); 6];
            state.record_hit(128, 128, 0);
            state.record_hit(192, 128, 0);
            state.tick(&layout, frame(early), &mut out);
            out[2].v
        };

        assert_ne!(
            still_at_origin(CometParams::PACE_MIN),
            0,
            "an unpaced short hop left too quickly"
        );
        assert_eq!(
            still_at_origin(CometParams::PACE_MAX),
            0,
            "a paced short hop had not got going"
        );
    }

    /// Redirected in flight onto a target just ahead of it, the comet's own
    /// speed carries it past that target before it settles back. With momentum
    /// turned off the same script stops short, which is what distinguishes the
    /// overshoot from the head simply being wide.
    #[test]
    fn momentum_swings_the_comet_wide_of_a_target_it_is_redirected_onto() {
        let layout = SliceLayout::new(ROW);
        let redirect = full_board_travel() / 2;

        let mut overshoot = false;
        let mut inertialess = false;
        for (momentum, reached) in [
            (CometParams::MOMENTUM_MAX, &mut overshoot),
            (CometParams::MOMENTUM_MIN, &mut inertialess),
        ] {
            let params = CometParams {
                momentum,
                ..CometParams::DEFAULT
            };
            let mut state = tuned::<8>(params);
            let mut out = [Hsv::default(); 6];
            state.record_hit(0, 128, 0);
            // Aimed past index 3, then pulled back onto index 2 mid-flight.
            state.record_hit(192, 128, 0);
            state.record_hit(128, 128, redirect);

            for step in 0..=full_board_travel() / 10 {
                state.tick(&layout, frame(redirect + step * 10), &mut out);
                *reached |= out[3].v != 0;
            }
        }

        assert!(
            overshoot,
            "momentum never carried the comet past its target"
        );
        assert!(
            !inertialess,
            "a comet with no momentum still overshot its target"
        );
    }

    /// The path just flown is lit behind the head, and only behind it.
    #[test]
    fn the_trail_lights_the_path_the_comet_came_from() {
        let layout = SliceLayout::new(ROW);
        let mut state = CometState::<8>::new();
        let mut out = [Hsv::default(); 6];

        state.record_hit(0, 128, 0);
        state.record_hit(128, 128, 0);
        state.tick(&layout, frame(full_board_travel()), &mut out);

        assert_ne!(out[2].v, 0, "the head is not at the key it flew to");
        assert_ne!(out[1].v, 0, "nothing was left along the path");
        assert!(
            out[1].v < out[2].v,
            "the trail is not dimmer than the head it trails"
        );
        assert_eq!(out[4].v, 0, "the trail ran out ahead of the comet");
    }

    /// A trail is optional, and switching it off leaves a bare head rather
    /// than smearing a stroke of zero length across the board.
    #[test]
    fn a_zero_trail_leaves_only_the_head() {
        let layout = SliceLayout::new(ROW);
        let mut state = tuned::<8>(CometParams {
            trail: 0,
            ..CometParams::DEFAULT
        });
        let mut out = [Hsv::default(); 6];

        state.record_hit(0, 128, 0);
        state.record_hit(128, 128, 0);
        state.tick(&layout, frame(full_board_travel()), &mut out);

        assert_ne!(out[2].v, 0);
        assert_eq!(out[1].v, 0, "a disabled trail still lit the path");
    }

    /// After a long pause the comet is gone, and the next press starts a new
    /// flight at that key instead of dragging the body across the board from
    /// wherever it died.
    #[test]
    fn a_press_after_the_fade_starts_over_at_the_key() {
        let layout = SliceLayout::new(ROW);
        let mut state = CometState::<8>::new();
        let mut out = [Hsv::default(); 6];

        state.record_hit(0, 128, 0);
        let long_after = full_board_travel() + scaled_ms(CometParams::DEFAULT.linger, 128) + 1_000;
        state.tick(&layout, frame(long_after), &mut out);
        assert!(
            out.iter().all(|pixel| *pixel == Hsv::default()),
            "the comet never faded out"
        );

        state.record_hit(255, 128, long_after);
        state.tick(&layout, frame(long_after), &mut out);
        assert_ne!(out[5].v, 0, "the restarted comet is not at the key");
        assert_eq!(out[0].v, 0, "the restarted comet flew in from the old key");
    }

    /// An animation clock that snaps backward -- what the peripheral half sees
    /// when the central reboots under it -- leaves every leg stamped in a
    /// future that will not arrive. Dropping them costs one press; keeping
    /// them would strand the comet until the clock climbed back.
    #[test]
    fn a_rewound_clock_drops_the_flight_instead_of_stranding_it() {
        let layout = SliceLayout::new(ROW);
        let mut state = CometState::<8>::new();
        let mut out = [Hsv::default(); 6];

        state.record_hit(0, 128, 100_000);
        state.tick(&layout, frame(100_000), &mut out);
        assert_ne!(out[0].v, 0);

        state.tick(&layout, frame(10), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));

        state.record_hit(255, 128, 10);
        state.tick(&layout, frame(10), &mut out);
        assert_ne!(out[5].v, 0, "the comet never recovered from the rewind");
    }

    #[test]
    fn out_of_range_parameters_are_declined_without_mutating() {
        let mut params = CometParams::DEFAULT;
        assert!(!params.set(0, CometParams::LAG_MIN - 1));
        assert!(!params.set(3, CometParams::TRAIL_MAX + 1));
        assert!(!params.set(CometParams::COUNT, 0));
        assert_eq!(params, CometParams::DEFAULT);
        assert_eq!(params.get(CometParams::COUNT), None);

        assert!(params.set(2, CometParams::MOMENTUM_UNIT));
        assert_eq!(params.get(2), Some(CometParams::MOMENTUM_UNIT));
        assert_eq!(CometState::<1>::new().params(), CometParams::DEFAULT);
        assert_eq!(CometParams::DEFAULTS[0], CometParams::DEFAULT.lag);
    }

    /// Sizing the ring at zero is legal for the const generic, so a press
    /// against it has to be dropped rather than indexing out of bounds.
    #[test]
    fn a_ring_with_no_legs_ignores_presses() {
        let layout = SliceLayout::new(ROW);
        let mut state = CometState::<0>::new();
        let mut out = [Hsv::default(); 6];
        state.record_hit(0, 128, 0);
        state.tick(&layout, frame(0), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }
}
