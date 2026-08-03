//! CROSSHAIR: a short row-and-column pulse centred on a pressed key.
//!
//! The four motions share the same topology-aware cross geometry. Distances
//! along each arm are normalized independently between the hit and that
//! arm's board edge, so an off-centre hit does not make a short arm finish
//! before a long one.

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;

/// Animation selected by [`CrosshairParams::motion`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CrosshairMotion {
    /// The complete row and column appear immediately and fade away.
    Flash = 0,
    /// Four streaks travel from the board edges toward the pressed key.
    Inward = 1,
    /// Four streaks travel from the pressed key toward the board edges.
    Outward = 2,
    /// The complete cross contracts from its edges toward the pressed key.
    Collapse = 3,
}

impl CrosshairMotion {
    const fn from_byte(value: u8) -> Self {
        match value {
            1 => Self::Inward,
            2 => Self::Outward,
            3 => Self::Collapse,
            _ => Self::Flash,
        }
    }
}

/// Runtime controls for [`CrosshairState`].
///
/// Every field is a byte so it can be exposed directly through a firmware
/// parameter protocol. `motion` maps to [`CrosshairMotion`] in declaration
/// order. Values changed through [`Self::set`] are range checked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CrosshairParams {
    /// 0 Flash, 1 Inward, 2 Outward, 3 Collapse.
    pub motion: u8,
    /// Base lifetime in units of 10 milliseconds.
    pub duration: u8,
    /// Coordinate tolerance perpendicular to each row/column arm.
    pub arm_half_width: u8,
    /// Width of a moving streak in normalized 0..=255 arm units.
    pub pulse_width: u8,
    /// Number of most recently recorded crosses that may be displayed.
    pub active_crosses: u8,
    /// Hue used by arm pixels.
    pub arm_hue: u8,
    /// Contrasting hue used by the exact LED that triggered a cross.
    pub key_hue: u8,
}

impl CrosshairParams {
    pub const DURATION_STEP_MS: u32 = 10;

    pub const MOTION_MIN: u8 = CrosshairMotion::Flash as u8;
    pub const MOTION_MAX: u8 = CrosshairMotion::Collapse as u8;
    pub const DURATION_MIN: u8 = 4;
    pub const DURATION_MAX: u8 = 255;
    pub const ARM_HALF_WIDTH_MIN: u8 = 0;
    pub const ARM_HALF_WIDTH_MAX: u8 = 48;
    pub const PULSE_WIDTH_MIN: u8 = 4;
    pub const PULSE_WIDTH_MAX: u8 = 192;
    pub const ACTIVE_CROSSES_MIN: u8 = 1;
    pub const ACTIVE_CROSSES_MAX: u8 = 16;
    pub const HUE_MIN: u8 = 0;
    pub const HUE_MAX: u8 = 255;

    pub const HUE_WARM: u8 = 16;
    pub const HUE_BLUE: u8 = 172;

    pub const DEFAULT: Self = Self {
        motion: CrosshairMotion::Flash as u8,
        duration: 16,
        arm_half_width: 8,
        pulse_width: 56,
        active_crosses: 4,
        arm_hue: Self::HUE_BLUE,
        key_hue: Self::HUE_WARM,
    };

    pub const COUNT: u8 = 7;

    pub const NAMES: [&'static str; Self::COUNT as usize] = [
        "Motion",
        "Duration x10ms",
        "Arm width",
        "Pulse width",
        "Crosses",
        "Arm hue",
        "Key hue",
    ];

    pub const MINS: [u8; Self::COUNT as usize] = [
        Self::MOTION_MIN,
        Self::DURATION_MIN,
        Self::ARM_HALF_WIDTH_MIN,
        Self::PULSE_WIDTH_MIN,
        Self::ACTIVE_CROSSES_MIN,
        Self::HUE_MIN,
        Self::HUE_MIN,
    ];

    pub const MAXES: [u8; Self::COUNT as usize] = [
        Self::MOTION_MAX,
        Self::DURATION_MAX,
        Self::ARM_HALF_WIDTH_MAX,
        Self::PULSE_WIDTH_MAX,
        Self::ACTIVE_CROSSES_MAX,
        Self::HUE_MAX,
        Self::HUE_MAX,
    ];

    pub const DEFAULTS: [u8; Self::COUNT as usize] = [
        Self::DEFAULT.motion,
        Self::DEFAULT.duration,
        Self::DEFAULT.arm_half_width,
        Self::DEFAULT.pulse_width,
        Self::DEFAULT.active_crosses,
        Self::DEFAULT.arm_hue,
        Self::DEFAULT.key_hue,
    ];

    /// Value of one parameter, or `None` for an unknown index.
    pub const fn get(&self, index: u8) -> Option<u8> {
        Some(match index {
            0 => self.motion,
            1 => self.duration,
            2 => self.arm_half_width,
            3 => self.pulse_width,
            4 => self.active_crosses,
            5 => self.arm_hue,
            6 => self.key_hue,
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
            0 => self.motion = value,
            1 => self.duration = value,
            2 => self.arm_half_width = value,
            3 => self.pulse_width = value,
            4 => self.active_crosses = value,
            5 => self.arm_hue = value,
            6 => self.key_hue = value,
            _ => return false,
        }
        true
    }

    /// Lifetime after applying the shared speed control.
    ///
    /// Speed 128 preserves the configured base duration. The bounded linear
    /// mapping ranges from 1.5x at speed 0 to approximately 0.5x at 255.
    fn duration_ms(&self, speed: u8) -> u32 {
        let base = self.duration.clamp(Self::DURATION_MIN, Self::DURATION_MAX) as u32
            * Self::DURATION_STEP_MS;
        (base * (384 - speed as u32) / 256).max(1)
    }
}

impl Default for CrosshairParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Copy, Clone)]
struct CrosshairHit {
    led_idx: usize,
    x: u8,
    y: u8,
    spawn_ms: u32,
    active: bool,
}

const EMPTY_HIT: CrosshairHit = CrosshairHit {
    led_idx: 0,
    x: 0,
    y: 0,
    spawn_ms: 0,
    active: false,
};

/// Ring of the `N` most recently recorded key hits and its current tuning.
pub struct CrosshairState<const N: usize> {
    hits: [CrosshairHit; N],
    next: usize,
    params: CrosshairParams,
}

impl<const N: usize> Default for CrosshairState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> CrosshairState<N> {
    pub const fn new() -> Self {
        Self::with_params(CrosshairParams::DEFAULT)
    }

    pub const fn with_params(params: CrosshairParams) -> Self {
        Self {
            hits: [EMPTY_HIT; N],
            next: 0,
            params,
        }
    }

    pub const fn params(&self) -> CrosshairParams {
        self.params
    }

    pub const fn set_params(&mut self, params: CrosshairParams) {
        self.params = params;
    }

    /// Record a press. `led_idx` identifies the exact key even when multiple
    /// LEDs share topology coordinates.
    pub fn record_hit(&mut self, led_idx: usize, x: u8, y: u8, timer_ms: u32) {
        if N == 0 {
            return;
        }
        self.hits[self.next] = CrosshairHit {
            led_idx,
            x,
            y,
            spawn_ms: timer_ms,
            active: true,
        };
        self.next = (self.next + 1) % N;
    }

    /// Render as a standalone sparse effect. Idle and non-arm pixels are black.
    pub fn tick<L: LedLayout>(&mut self, layout: &L, frame: FrameParams<'_>, out: &mut [Hsv]) {
        self.render(layout, frame, out);
    }

    /// Render as a sparse overlay. Its value is direct layer coverage.
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
        if N == 0 {
            return;
        }

        let duration = self.params.duration_ms(frame.speed);
        let remembered = (self.params.active_crosses as usize)
            .clamp(CrosshairParams::ACTIVE_CROSSES_MIN as usize, N);

        for led_idx in 0..layout.count().min(out.len()) {
            let (x, y) = layout.position(led_idx);
            let mut arm_value = 0;
            let mut key_value = 0;
            let mut active_key = false;

            // `next - 1` is newest. Limiting this walk, instead of limiting
            // active slots globally, makes active_crosses=1 truncate an old
            // animation immediately when a new press is recorded.
            for age in 0..remembered {
                let hit_index = (self.next + N - 1 - age) % N;
                let hit = self.hits[hit_index];
                if !hit.active {
                    continue;
                }
                let elapsed = frame.timer_ms.wrapping_sub(hit.spawn_ms);
                if elapsed >= duration {
                    self.hits[hit_index].active = false;
                    continue;
                }

                let value = value_at(self.params, hit, x, y, elapsed, duration);
                if led_idx == hit.led_idx {
                    active_key = true;
                    key_value = key_value.max(value);
                } else {
                    arm_value = arm_value.max(value);
                }
            }

            let (hue, value) = if active_key {
                // A cross centre wins over every arm crossing this pixel,
                // including while its own moving pulse is elsewhere.
                (self.params.key_hue, key_value.max(arm_value))
            } else {
                (self.params.arm_hue, arm_value)
            };
            if value != 0 {
                out[led_idx] = Hsv::new(hue, frame.sat, scale255(value, frame.val));
            }
        }
    }
}

fn value_at(
    params: CrosshairParams,
    hit: CrosshairHit,
    x: u8,
    y: u8,
    elapsed: u32,
    duration: u32,
) -> u8 {
    let width = params.arm_half_width.clamp(
        CrosshairParams::ARM_HALF_WIDTH_MIN,
        CrosshairParams::ARM_HALF_WIDTH_MAX,
    );
    let dx = abs_diff(x, hit.x);
    let dy = abs_diff(y, hit.y);
    let horizontal = dy <= width;
    let vertical = dx <= width;
    if !horizontal && !vertical {
        return 0;
    }

    let progress = ((elapsed as u64 * 255) / duration as u64) as u8;
    let motion = CrosshairMotion::from_byte(params.motion);
    let mut value = 0;

    if horizontal {
        let along = normalized_distance(x, hit.x);
        value = value.max(arm_value(
            motion,
            along,
            dy,
            width,
            params.pulse_width,
            progress,
        ));
    }
    if vertical {
        let along = normalized_distance(y, hit.y);
        value = value.max(arm_value(
            motion,
            along,
            dx,
            width,
            params.pulse_width,
            progress,
        ));
    }
    value
}

/// Distance from `hit` on the corresponding half-arm, normalized so the hit
/// is 0 and either board edge is 255. A hit exactly on an edge has a zero-span
/// arm; its only point is the hit itself.
fn normalized_distance(position: u8, hit: u8) -> u8 {
    if position < hit {
        ((hit as u16 - position as u16) * 255 / hit as u16) as u8
    } else if position > hit {
        let span = 255u16 - hit as u16;
        ((position as u16 - hit as u16) * 255 / span) as u8
    } else {
        0
    }
}

fn arm_value(
    motion: CrosshairMotion,
    along: u8,
    transverse: u8,
    arm_width: u8,
    pulse_width: u8,
    progress: u8,
) -> u8 {
    let transverse_value = soft_edge(transverse, arm_width);
    let longitudinal = match motion {
        CrosshairMotion::Flash => 255 - progress,
        CrosshairMotion::Inward => {
            let centre = 255 - travel_progress(progress);
            scale255(
                streak(along, centre, pulse_width),
                motion_envelope(progress),
            )
        }
        CrosshairMotion::Outward => scale255(
            streak(along, travel_progress(progress), pulse_width),
            motion_envelope(progress),
        ),
        CrosshairMotion::Collapse => {
            let boundary = 255 - progress;
            let contracted = if along <= boundary {
                let softness = pulse_width.clamp(
                    CrosshairParams::PULSE_WIDTH_MIN,
                    CrosshairParams::PULSE_WIDTH_MAX,
                );
                if boundary - along >= softness {
                    255
                } else {
                    (((boundary - along) as u16 + 1) * 255 / (softness as u16 + 1)) as u8
                }
            } else {
                0
            };
            scale255(contracted, 255 - progress)
        }
    };
    scale255(longitudinal, transverse_value)
}

fn streak(position: u8, centre: u8, width: u8) -> u8 {
    let width = width.clamp(
        CrosshairParams::PULSE_WIDTH_MIN,
        CrosshairParams::PULSE_WIDTH_MAX,
    );
    let distance = abs_diff(position, centre);
    if distance > width {
        0
    } else {
        255 - (distance as u16 * 255 / (width as u16 + 1)) as u8
    }
}

/// Moving streaks finish their travel halfway through the lifetime, then
/// briefly fade at the destination. Starting fully lit is deliberate: at the
/// fastest useful durations a 25 fps renderer may only sample two frames, so
/// both the origin and destination still need a chance to be visible.
fn travel_progress(progress: u8) -> u8 {
    progress.saturating_mul(2)
}

fn motion_envelope(progress: u8) -> u8 {
    if progress < 128 {
        255
    } else {
        (255 - progress).saturating_mul(2)
    }
}

fn soft_edge(distance: u8, half_width: u8) -> u8 {
    if half_width == 0 {
        return if distance == 0 { 255 } else { 0 };
    }
    if distance > half_width {
        0
    } else {
        255 - (distance as u16 * 255 / (half_width as u16 + 1)) as u8
    }
}

const fn abs_diff(a: u8, b: u8) -> u8 {
    a.abs_diff(b)
}

/// Multiply two conventional 0..=255 fractions while preserving endpoints.
const fn scale255(value: u8, scale: u8) -> u8 {
    ((value as u32 * scale as u32 + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SliceLayout;
    use crate::palette::CARNIVAL;

    const CROSS: &[(u8, u8)] = &[
        (0, 128),
        (64, 128),
        (128, 128),
        (192, 128),
        (255, 128),
        (128, 0),
        (128, 64),
        (128, 192),
        (128, 255),
        (20, 20),
    ];

    fn frame(timer_ms: u32) -> FrameParams<'static> {
        FrameParams {
            palette: &CARNIVAL,
            speed: 128,
            sat: 220,
            val: 255,
            timer_ms,
        }
    }

    fn state_with(motion: CrosshairMotion, active_crosses: u8) -> CrosshairState<4> {
        CrosshairState::with_params(CrosshairParams {
            motion: motion as u8,
            duration: 20,
            arm_half_width: 0,
            pulse_width: 40,
            active_crosses,
            ..CrosshairParams::DEFAULT
        })
    }

    #[test]
    fn parameter_metadata_matches_defaults_and_rejects_invalid_values() {
        let mut params = CrosshairParams::DEFAULT;
        assert_eq!(CrosshairParams::COUNT, 7);
        for i in 0..CrosshairParams::COUNT {
            assert_eq!(params.get(i), Some(CrosshairParams::DEFAULTS[i as usize]));
            assert!(CrosshairParams::DEFAULTS[i as usize] >= CrosshairParams::MINS[i as usize]);
            assert!(CrosshairParams::DEFAULTS[i as usize] <= CrosshairParams::MAXES[i as usize]);
        }
        assert_eq!(params.get(CrosshairParams::COUNT), None);
        assert!(!params.set(CrosshairParams::COUNT, 0));
        assert!(!params.set(0, CrosshairParams::MOTION_MAX + 1));
        assert!(!params.set(1, CrosshairParams::DURATION_MIN - 1));
        assert_eq!(params, CrosshairParams::DEFAULT);
        assert!(params.set(0, CrosshairMotion::Inward as u8));
        assert_eq!(params.motion, CrosshairMotion::Inward as u8);
    }

    #[test]
    fn one_active_cross_truncates_the_previous_hit_immediately() {
        let layout = SliceLayout::new(CROSS);
        let mut state = state_with(CrosshairMotion::Flash, 1);
        let mut out = [Hsv::default(); CROSS.len()];
        state.record_hit(0, 0, 128, 0);
        state.record_hit(9, 20, 20, 20);
        state.tick(&layout, frame(20), &mut out);
        assert_eq!(
            out[1],
            Hsv::default(),
            "the older horizontal arm was not truncated"
        );
        assert_eq!(out[9].h, CrosshairParams::DEFAULT.key_hue);
        assert!(out[9].v > 0);
    }

    #[test]
    fn exact_key_has_distinct_hue_and_wins_an_overlap() {
        let layout = SliceLayout::new(CROSS);
        let mut state = state_with(CrosshairMotion::Outward, 2);
        let mut out = [Hsv::default(); CROSS.len()];
        state.record_hit(0, 0, 128, 0);
        state.record_hit(2, 128, 128, 0);
        // While moving, the newer hit's own pulse has left its centre, while
        // the older hit's rightward arm is crossing that centre.
        state.tick(&layout, frame(50), &mut out);
        assert_eq!(out[2].h, state.params().key_hue);
        assert_eq!(out[1].h, state.params().arm_hue);
        assert!(out[2].v > 0 && out[1].v > 0);
    }

    #[test]
    fn inward_and_outward_move_in_opposite_directions() {
        let layout = SliceLayout::new(CROSS);
        let mut out = [Hsv::default(); CROSS.len()];

        let build = |motion: CrosshairMotion| {
            CrosshairState::<4>::with_params(CrosshairParams {
                motion: motion as u8,
                duration: CrosshairParams::DEFAULT.duration,
                arm_half_width: 0,
                pulse_width: 40,
                active_crosses: 1,
                ..CrosshairParams::DEFAULT
            })
        };

        let mut inward = build(CrosshairMotion::Inward);
        inward.record_hit(2, 128, 128, 0);
        inward.tick(&layout, frame(1), &mut out); // pulse is still near the edge
        let inward_edge_early = out[0].v;
        let inward_center_early = out[2].v;
        inward.tick(&layout, frame(100), &mut out); // pulse has reached the key
        assert!(inward_edge_early > inward_center_early);
        assert!(out[2].v > out[0].v);

        let mut outward = build(CrosshairMotion::Outward);
        outward.record_hit(2, 128, 128, 0);
        outward.tick(&layout, frame(1), &mut out);
        let outward_edge_early = out[0].v;
        let outward_center_early = out[2].v;
        outward.tick(&layout, frame(100), &mut out);
        assert!(outward_center_early > outward_edge_early);
        assert!(out[0].v > out[2].v);
    }

    #[test]
    fn idle_and_expired_frames_are_black_in_both_render_modes() {
        let layout = SliceLayout::new(CROSS);
        let mut state = state_with(CrosshairMotion::Flash, 1);
        let mut out = [Hsv::new(1, 2, 3); CROSS.len()];
        state.tick(&layout, frame(0), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
        state.record_hit(2, 128, 128, 0);
        state.tick_layer(&layout, frame(200), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }

    #[test]
    fn zero_hit_capacity_is_a_noop_without_panicking() {
        let layout = SliceLayout::new(CROSS);
        let mut state = CrosshairState::<0>::new();
        let mut out = [Hsv::new(1, 2, 3); CROSS.len()];
        state.record_hit(2, 128, 128, 0);
        state.tick(&layout, frame(0), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }

    #[test]
    fn timer_wrap_is_forward_progress_and_large_backward_jump_expires() {
        let layout = SliceLayout::new(CROSS);
        let mut state = state_with(CrosshairMotion::Flash, 1);
        let mut out = [Hsv::default(); CROSS.len()];
        state.record_hit(2, 128, 128, u32::MAX - 20);
        state.tick(&layout, frame(10), &mut out);
        assert!(
            out[2].v > 0,
            "a short wrapping elapsed time must remain active"
        );

        state.record_hit(2, 128, 128, 50_000);
        state.tick(&layout, frame(100), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }

    #[test]
    fn global_speed_shortens_the_configured_base_duration() {
        let layout = SliceLayout::new(CROSS);
        let mut slow = state_with(CrosshairMotion::Flash, 1);
        let mut fast = state_with(CrosshairMotion::Flash, 1);
        let mut slow_out = [Hsv::default(); CROSS.len()];
        let mut fast_out = [Hsv::default(); CROSS.len()];
        slow.record_hit(2, 128, 128, 0);
        fast.record_hit(2, 128, 128, 0);

        slow.tick(
            &layout,
            FrameParams {
                speed: 0,
                timer_ms: 150,
                ..frame(0)
            },
            &mut slow_out,
        );
        fast.tick(
            &layout,
            FrameParams {
                speed: 255,
                timer_ms: 150,
                ..frame(0)
            },
            &mut fast_out,
        );
        assert!(slow_out[2].v > 0);
        assert_eq!(fast_out[2], Hsv::default());
    }

    #[test]
    fn fastest_moving_pulses_reach_their_destination_at_25_fps() {
        let layout = SliceLayout::new(CROSS);
        let mut out = [Hsv::default(); CROSS.len()];
        let fast_frame = |timer_ms| FrameParams {
            speed: 255,
            timer_ms,
            ..frame(0)
        };
        let build = |motion: CrosshairMotion| {
            CrosshairState::<4>::with_params(CrosshairParams {
                motion: motion as u8,
                arm_half_width: 0,
                pulse_width: 40,
                active_crosses: 1,
                ..CrosshairParams::DEFAULT
            })
        };

        let mut inward = build(CrosshairMotion::Inward);
        inward.record_hit(2, 128, 128, 0);
        inward.tick(&layout, fast_frame(0), &mut out);
        assert!(out[0].v > 0, "inward pulse did not start at the edge");
        inward.tick(&layout, fast_frame(40), &mut out);
        assert!(out[2].v > 0, "inward pulse skipped the pressed key");

        let mut outward = build(CrosshairMotion::Outward);
        outward.record_hit(2, 128, 128, 0);
        outward.tick(&layout, fast_frame(0), &mut out);
        assert!(out[2].v > 0, "outward pulse did not start at the key");
        outward.tick(&layout, fast_frame(40), &mut out);
        assert!(out[0].v > 0, "outward pulse skipped the board edge");
    }
}
