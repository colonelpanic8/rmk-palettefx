//! SHOCKWAVE: expanding rings launched by key presses.

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::math::{abs_half_diff, sqrt16};
use crate::palette::interp_color;

const BASE_DURATION_MS: u32 = 900;
const RING_HALF_WIDTH: u8 = 9;

#[derive(Copy, Clone)]
struct ShockwaveHit {
    x: u8,
    y: u8,
    max_radius: u8,
    spawn_ms: u32,
    palette_pos: u8,
    active: bool,
}

const EMPTY_HIT: ShockwaveHit = ShockwaveHit {
    x: 0,
    y: 0,
    max_radius: 0,
    spawn_ms: 0,
    palette_pos: 0,
    active: false,
};

/// Ring of the `N` most recent shockwaves.
pub struct ShockwaveState<const N: usize> {
    hits: [ShockwaveHit; N],
    next: usize,
    color_step: u8,
}

impl<const N: usize> Default for ShockwaveState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ShockwaveState<N> {
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
        self.hits[self.next] = ShockwaveHit {
            x,
            y,
            max_radius: (0..layout.count())
                .map(|index| {
                    let (led_x, led_y) = layout.position(index);
                    radial_distance(x, y, led_x, led_y)
                })
                .max()
                .unwrap_or(0),
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
                let value = wave_value(*hit, x, y, progress);
                if value > strongest {
                    strongest = value;
                    palette_pos = hit.palette_pos.wrapping_add(progress / 3);
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

fn wave_value(hit: ShockwaveHit, x: u8, y: u8, progress: u8) -> u8 {
    let distance = radial_distance(x, y, hit.x, hit.y);
    let radius = scale255(hit.max_radius, progress);
    let delta = distance.abs_diff(radius);
    if delta > RING_HALF_WIDTH {
        return 0;
    }
    let band = 255 - (delta as u16 * 255 / (RING_HALF_WIDTH as u16 + 1)) as u8;
    scale255(band, 255 - progress)
}

fn radial_distance(ax: u8, ay: u8, bx: u8, by: u8) -> u8 {
    let dx = abs_half_diff(ax, bx);
    let dy = abs_half_diff(ay, by);
    sqrt16(dx as u16 * dx as u16 + dy as u16 * dy as u16)
}

/// Speed 128 preserves the base duration. The shared speed range maps from
/// 1.5x at zero to approximately 0.5x at 255.
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

    const POSITIONS: &[(u8, u8)] = &[(128, 128), (248, 128), (248, 248)];

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
    fn ring_moves_away_from_the_pressed_key() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = ShockwaveState::<4>::new();
        let mut out = [Hsv::default(); 3];
        state.record_hit(&layout, 128, 128, 0);

        state.tick(&layout, frame(0), &mut out);
        assert_ne!(out[0].v, 0);
        assert_eq!(out[1].v, 0);

        // The second LED is 60 units away and the diagonal LED is 84 units
        // away in the effect's half-distance grid.
        let at_radius_60 = duration_ms(128) * 60 / 84;
        state.tick(&layout, frame(at_radius_60), &mut out);
        assert_eq!(out[0].v, 0);
        assert_ne!(out[1].v, 0);
        assert_eq!(out[2].v, 0);
    }

    #[test]
    fn expired_ring_is_transparent() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = ShockwaveState::<1>::new();
        let mut out = [Hsv::default(); 3];
        state.record_hit(&layout, 128, 128, 0);
        state.tick(&layout, frame(duration_ms(128)), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }
}
