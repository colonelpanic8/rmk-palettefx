//! KEYFALL: streaks launched from pressed keys, falling toward the board edge
//! [`KeyfallParams::gravity`] selects.

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::palette::interp_color;

const BASE_DURATION_MS: u32 = 760;
const COLUMN_HALF_WIDTH: u8 = 11;
const TRAIL_LENGTH: u8 = 58;
const TRAVEL_END: u8 = 192;

/// Direction streaks fall, selected by [`KeyfallParams::gravity`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum KeyfallGravity {
    /// Toward the bottom edge, the original behaviour.
    Down = 0,
    /// Toward the top edge.
    Up = 1,
    /// Toward the left edge.
    Left = 2,
    /// Toward the right edge.
    Right = 3,
}

impl KeyfallGravity {
    const fn from_byte(value: u8) -> Self {
        match value {
            1 => Self::Up,
            2 => Self::Left,
            3 => Self::Right,
            _ => Self::Down,
        }
    }

    /// A board position as `(along, across)`: `along` grows in the falling
    /// direction and `across` is the axis a streak's width is measured on.
    ///
    /// Mirroring an axis rather than carrying a sign keeps every comparison in
    /// the renderer the same for all four directions -- a streak still runs
    /// from a lower `along` to a higher one whichever way gravity points.
    const fn project(self, x: u8, y: u8) -> (u8, u8) {
        match self {
            Self::Down => (y, x),
            Self::Up => (255 - y, x),
            Self::Right => (x, y),
            Self::Left => (255 - x, y),
        }
    }
}

/// Runtime controls for [`KeyfallState`].
///
/// Every field is a byte so it can be exposed directly through a firmware
/// parameter protocol. `gravity` maps to [`KeyfallGravity`] in declaration
/// order. Values changed through [`Self::set`] are range checked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KeyfallParams {
    /// 0 down, 1 up, 2 left, 3 right.
    pub gravity: u8,
}

impl KeyfallParams {
    pub const GRAVITY_MIN: u8 = KeyfallGravity::Down as u8;
    pub const GRAVITY_MAX: u8 = KeyfallGravity::Right as u8;

    /// The tuned defaults. Down is what the effect did before it was
    /// steerable, so a board that never touches the parameter is unchanged.
    pub const DEFAULT: Self = Self {
        gravity: KeyfallGravity::Down as u8,
    };

    pub const COUNT: u8 = 1;

    pub const NAMES: [&'static str; Self::COUNT as usize] = ["Gravity"];

    pub const MINS: [u8; Self::COUNT as usize] = [Self::GRAVITY_MIN];

    pub const MAXES: [u8; Self::COUNT as usize] = [Self::GRAVITY_MAX];

    pub const DEFAULTS: [u8; Self::COUNT as usize] = [Self::DEFAULT.gravity];

    /// Value of one parameter, or `None` for an unknown index.
    pub const fn get(&self, index: u8) -> Option<u8> {
        Some(match index {
            0 => self.gravity,
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
            0 => self.gravity = value,
            _ => return false,
        }
        true
    }

    /// The selected direction. Out-of-range bytes cannot reach here through
    /// [`Self::set`], and a hand-built struct falls back to Down.
    const fn direction(&self) -> KeyfallGravity {
        KeyfallGravity::from_byte(self.gravity)
    }
}

impl Default for KeyfallParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Copy, Clone)]
struct FallingHit {
    /// The pressed key, projected onto the axes of the gravity that was in
    /// force when the press was recorded.
    along: u8,
    across: u8,
    /// Where the board ends in the falling direction, same projection.
    end: u8,
    /// Held per hit so retuning gravity aims the next streak rather than
    /// snapping the ones already in flight onto a new axis.
    gravity: KeyfallGravity,
    spawn_ms: u32,
    palette_pos: u8,
    active: bool,
}

const EMPTY_HIT: FallingHit = FallingHit {
    along: 0,
    across: 0,
    end: 0,
    gravity: KeyfallGravity::Down,
    spawn_ms: 0,
    palette_pos: 0,
    active: false,
};

/// Ring of the `N` most recent falling key streaks and its current tuning.
pub struct KeyfallState<const N: usize> {
    hits: [FallingHit; N],
    next: usize,
    color_step: u8,
    params: KeyfallParams,
}

impl<const N: usize> Default for KeyfallState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> KeyfallState<N> {
    pub const fn new() -> Self {
        Self::with_params(KeyfallParams::DEFAULT)
    }

    /// State pre-tuned with `params`; see [`KeyfallState::set_params`] for
    /// changing them on a running effect.
    pub const fn with_params(params: KeyfallParams) -> Self {
        Self {
            hits: [EMPTY_HIT; N],
            next: 0,
            color_step: 0,
            params,
        }
    }

    /// Retune a running effect. Streaks already falling keep the direction
    /// they launched with and finish where they were headed.
    pub const fn set_params(&mut self, params: KeyfallParams) {
        self.params = params;
    }

    /// The tuning currently in effect.
    pub const fn params(&self) -> KeyfallParams {
        self.params
    }

    pub fn record_hit<L: LedLayout>(&mut self, layout: &L, x: u8, y: u8, timer_ms: u32) {
        if N == 0 {
            return;
        }
        let gravity = self.params.direction();
        let (along, across) = gravity.project(x, y);
        self.hits[self.next] = FallingHit {
            along,
            across,
            end: far_edge(layout, gravity).max(along),
            gravity,
            spawn_ms: timer_ms,
            palette_pos: x.wrapping_add(y / 2).wrapping_add(self.color_step),
            active: true,
        };
        self.next = (self.next + 1) % N;
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
        let duration = duration_ms(frame.speed);
        let mut progress = [None; N];
        for (slot, hit) in self.hits.iter_mut().enumerate() {
            if !hit.active {
                continue;
            }
            let elapsed = frame.timer_ms.wrapping_sub(hit.spawn_ms);
            if elapsed >= duration {
                hit.active = false;
            } else {
                progress[slot] = Some(((elapsed as u64 * 255) / duration as u64) as u8);
            }
        }

        for (led_idx, pixel) in out.iter_mut().take(layout.count()).enumerate() {
            let (x, y) = layout.position(led_idx);
            let mut strongest = 0;
            let mut palette_pos = 0;
            for (slot, hit) in self.hits.iter().enumerate() {
                let Some(progress) = progress[slot] else {
                    continue;
                };
                let value = falling_value(*hit, x, y, progress);
                if value > strongest {
                    strongest = value;
                    palette_pos = hit.palette_pos.wrapping_add(progress / 2);
                }
            }
            if strongest != 0 {
                *pixel = interp_color(
                    frame.palette,
                    palette_pos,
                    frame.sat,
                    scale255(strongest, frame.val),
                );
            }
        }
    }
}

/// The largest `along` coordinate any LED has, i.e. where the board ends in
/// the falling direction. Read from the layout rather than assumed to be 255
/// so a streak reaches the last row of emitters at the same point in its
/// lifetime whichever way it falls.
fn far_edge<L: LedLayout>(layout: &L, gravity: KeyfallGravity) -> u8 {
    let mut edge = 0;
    for index in 0..layout.count() {
        let (x, y) = layout.position(index);
        let (along, _) = gravity.project(x, y);
        if along > edge {
            edge = along;
        }
    }
    edge
}

fn falling_value(hit: FallingHit, x: u8, y: u8, progress: u8) -> u8 {
    let (along, across) = hit.gravity.project(x, y);
    let offset = across.abs_diff(hit.across);
    if offset > COLUMN_HALF_WIDTH || along < hit.along {
        return 0;
    }

    // Reach the far edge after three quarters of the lifetime, then hold
    // the streak there while it fades. This avoids a visually abrupt cutoff.
    let travel = if progress >= TRAVEL_END {
        255
    } else {
        (progress as u16 * 255 / TRAVEL_END as u16) as u8
    };
    let head = hit.along as u16 + ((hit.end - hit.along) as u16 * travel as u16 / 255);
    if along as u16 > head {
        return 0;
    }
    let behind = (head - along as u16).min(u8::MAX as u16) as u8;
    if behind > TRAIL_LENGTH {
        return 0;
    }

    let width = soft_edge(offset, COLUMN_HALF_WIDTH);
    let length = 255 - (behind as u16 * 255 / (TRAIL_LENGTH as u16 + 1)) as u8;
    let envelope = if progress < TRAVEL_END {
        255
    } else {
        ((255 - progress) as u16 * 255 / (255 - TRAVEL_END) as u16) as u8
    };
    scale255(scale255(width, length), envelope)
}

fn soft_edge(distance: u8, width: u8) -> u8 {
    if distance > width {
        0
    } else {
        255 - (distance as u16 * 255 / (width as u16 + 1)) as u8
    }
}

fn duration_ms(speed: u8) -> u32 {
    (BASE_DURATION_MS * (384 - speed as u32) / 256).max(1)
}

const fn scale255(value: u8, scale: u8) -> u8 {
    ((value as u32 * scale as u32 + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;
    use crate::palette::CARNIVAL;

    const POSITIONS: &[(u8, u8)] = &[(100, 10), (100, 100), (160, 100), (100, 0)];

    fn frame(timer_ms: u32) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed: 128,
            sat: 255,
            val: 255,
            timer_ms,
        }
    }

    /// Just after the streak's head reaches the layout's far edge. The extra
    /// buckets absorb the frame-progress division's rounding.
    fn head_at_edge() -> u32 {
        duration_ms(128) * (TRAVEL_END as u32 + 2) / 255
    }

    fn tuned<const N: usize>(gravity: KeyfallGravity) -> KeyfallState<N> {
        KeyfallState::with_params(KeyfallParams {
            gravity: gravity as u8,
        })
    }

    #[test]
    fn streak_falls_below_the_key_without_spreading_sideways_or_upward() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = KeyfallState::<4>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(&layout, 100, 10, 0);

        state.tick(&layout, frame(head_at_edge()), &mut out);
        assert_ne!(out[1].v, 0);
        assert_eq!(out[2].v, 0);
        assert_eq!(out[3].v, 0);
    }

    #[test]
    fn expired_streak_is_transparent() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = KeyfallState::<1>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(&layout, 100, 10, 0);
        state.tick(&layout, frame(duration_ms(128)), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }

    /// Each gravity lights the neighbour on its own side of the pressed key
    /// and leaves the other three dark, so the parameter really does steer the
    /// streak rather than only renaming the same fall.
    #[test]
    fn each_gravity_carries_the_streak_to_its_own_edge() {
        // A plus sign around the centre: one neighbour per direction.
        const CROSS: &[(u8, u8)] = &[(128, 128), (128, 255), (128, 0), (0, 128), (255, 128)];
        let layout = SliceLayout::new(CROSS);

        for (gravity, lit) in [
            (KeyfallGravity::Down, 1),
            (KeyfallGravity::Up, 2),
            (KeyfallGravity::Left, 3),
            (KeyfallGravity::Right, 4),
        ] {
            let mut state = tuned::<1>(gravity);
            let mut out = [Hsv::default(); 5];
            state.record_hit(&layout, 128, 128, 0);
            state.tick(&layout, frame(head_at_edge()), &mut out);

            assert_ne!(out[lit].v, 0, "{gravity:?} never reached its neighbour");
            for (index, pixel) in out.iter().enumerate().skip(1) {
                if index != lit {
                    assert_eq!(pixel.v, 0, "{gravity:?} lit LED {index}");
                }
            }
        }
    }

    /// Retuning aims the next press. A streak already falling keeps its own
    /// direction, which is what stops a live parameter change from bending
    /// mid-flight streaks sideways.
    #[test]
    fn streaks_keep_the_gravity_they_launched_with() {
        const COLUMN_AND_ROW: &[(u8, u8)] = &[(128, 128), (128, 255), (255, 128)];
        let layout = SliceLayout::new(COLUMN_AND_ROW);
        let mut state = KeyfallState::<2>::new();
        let mut out = [Hsv::default(); 3];

        state.record_hit(&layout, 128, 128, 0);
        state.set_params(KeyfallParams {
            gravity: KeyfallGravity::Right as u8,
        });
        state.record_hit(&layout, 128, 128, 0);

        state.tick(&layout, frame(head_at_edge()), &mut out);
        assert_ne!(out[1].v, 0, "the downward streak stopped falling");
        assert_ne!(out[2].v, 0, "the rightward streak never launched");
    }

    #[test]
    fn out_of_range_parameters_are_declined_without_mutating() {
        let mut params = KeyfallParams::DEFAULT;
        assert!(!params.set(0, KeyfallParams::GRAVITY_MAX + 1));
        assert!(!params.set(KeyfallParams::COUNT, 0));
        assert_eq!(params, KeyfallParams::DEFAULT);
        assert_eq!(params.get(KeyfallParams::COUNT), None);

        assert!(params.set(0, KeyfallGravity::Left as u8));
        assert_eq!(params.get(0), Some(KeyfallGravity::Left as u8));
        assert_eq!(params.direction(), KeyfallGravity::Left);
    }

    /// The default has to reproduce the tuning that used to be compiled in, so
    /// adding the parameter changed nothing on a board that never sets it.
    #[test]
    fn the_default_gravity_is_still_down() {
        assert_eq!(KeyfallParams::DEFAULT.direction(), KeyfallGravity::Down);
        assert_eq!(KeyfallState::<1>::new().params(), KeyfallParams::DEFAULT);
        assert_eq!(KeyfallParams::DEFAULTS, [KeyfallParams::DEFAULT.gravity]);
    }
}
