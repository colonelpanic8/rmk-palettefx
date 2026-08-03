//! KEYFALL: downward-falling streaks launched from pressed keys.

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::palette::interp_color;

const BASE_DURATION_MS: u32 = 760;
const COLUMN_HALF_WIDTH: u8 = 11;
const TRAIL_LENGTH: u8 = 58;
const TRAVEL_END: u8 = 192;

#[derive(Copy, Clone)]
struct FallingHit {
    x: u8,
    y: u8,
    bottom: u8,
    spawn_ms: u32,
    palette_pos: u8,
    active: bool,
}

const EMPTY_HIT: FallingHit = FallingHit {
    x: 0,
    y: 0,
    bottom: 0,
    spawn_ms: 0,
    palette_pos: 0,
    active: false,
};

/// Ring of the `N` most recent falling key streaks.
pub struct KeyfallState<const N: usize> {
    hits: [FallingHit; N],
    next: usize,
    color_step: u8,
}

impl<const N: usize> Default for KeyfallState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> KeyfallState<N> {
    pub const fn new() -> Self {
        Self {
            hits: [EMPTY_HIT; N],
            next: 0,
            color_step: 0,
        }
    }

    pub fn record_hit<L: LedLayout>(&mut self, layout: &L, x: u8, y: u8, timer_ms: u32) {
        if N == 0 {
            return;
        }
        self.hits[self.next] = FallingHit {
            x,
            y,
            bottom: layout.y_max().max(y),
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

fn falling_value(hit: FallingHit, x: u8, y: u8, progress: u8) -> u8 {
    let dx = x.abs_diff(hit.x);
    if dx > COLUMN_HALF_WIDTH || y < hit.y {
        return 0;
    }

    // Reach the lower edge after three quarters of the lifetime, then hold
    // the streak there while it fades. This avoids a visually abrupt cutoff.
    let travel = if progress >= TRAVEL_END {
        255
    } else {
        (progress as u16 * 255 / TRAVEL_END as u16) as u8
    };
    let head_y = hit.y as u16 + ((hit.bottom - hit.y) as u16 * travel as u16 / 255);
    if y as u16 > head_y {
        return 0;
    }
    let behind = (head_y - y as u16).min(u8::MAX as u16) as u8;
    if behind > TRAIL_LENGTH {
        return 0;
    }

    let across = soft_edge(dx, COLUMN_HALF_WIDTH);
    let along = 255 - (behind as u16 * 255 / (TRAIL_LENGTH as u16 + 1)) as u8;
    let envelope = if progress < TRAVEL_END {
        255
    } else {
        ((255 - progress) as u16 * 255 / (255 - TRAVEL_END) as u16) as u8
    };
    scale255(scale255(across, along), envelope)
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

    #[test]
    fn streak_falls_below_the_key_without_spreading_sideways_or_upward() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = KeyfallState::<4>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(&layout, 100, 10, 0);

        // Sample just after the streak reaches the layout's actual bottom.
        // The extra buckets absorb the frame-progress division's rounding.
        let when_head_reaches_second_key = duration_ms(128) * (TRAVEL_END as u32 + 2) / 255;
        state.tick(&layout, frame(when_head_reaches_second_key), &mut out);
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
}
