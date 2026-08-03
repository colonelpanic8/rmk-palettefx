//! TRACER: fading nodes and segments connecting consecutive key presses.

use super::FrameParams;
use crate::color::Hsv;
use crate::layout::LedLayout;
use crate::math::sqrt16;
use crate::palette::interp_color;

const BASE_DURATION_MS: u32 = 850;
const MAX_CONNECT_GAP_MS: u32 = 700;
const LINE_HALF_WIDTH: u8 = 4;
const NODE_RADIUS: u8 = 6;

#[derive(Copy, Clone)]
struct Segment {
    from_x: u8,
    from_y: u8,
    to_x: u8,
    to_y: u8,
    spawn_ms: u32,
    palette_pos: u8,
    active: bool,
}

const EMPTY_SEGMENT: Segment = Segment {
    from_x: 0,
    from_y: 0,
    to_x: 0,
    to_y: 0,
    spawn_ms: 0,
    palette_pos: 0,
    active: false,
};

#[derive(Copy, Clone)]
struct LastHit {
    x: u8,
    y: u8,
    spawn_ms: u32,
}

/// Ring of recent typing-path segments. A disconnected press is represented
/// by a degenerate segment, so the first key still appears as a bright node.
pub struct TracerState<const N: usize> {
    segments: [Segment; N],
    next: usize,
    last: Option<LastHit>,
    color_step: u8,
}

impl<const N: usize> Default for TracerState<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TracerState<N> {
    pub const fn new() -> Self {
        Self {
            segments: [EMPTY_SEGMENT; N],
            next: 0,
            last: None,
            color_step: 0,
        }
    }

    pub fn record_hit(&mut self, x: u8, y: u8, timer_ms: u32) {
        if N == 0 {
            return;
        }
        let (from_x, from_y) = self
            .last
            .filter(|last| timer_ms.wrapping_sub(last.spawn_ms) <= MAX_CONNECT_GAP_MS)
            .map(|last| (last.x, last.y))
            .unwrap_or((x, y));
        self.segments[self.next] = Segment {
            from_x,
            from_y,
            to_x: x,
            to_y: y,
            spawn_ms: timer_ms,
            palette_pos: x.wrapping_add(y / 2).wrapping_add(self.color_step),
            active: true,
        };
        self.next = (self.next + 1) % N;
        self.last = Some(LastHit {
            x,
            y,
            spawn_ms: timer_ms,
        });
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
        let mut fade = [0; N];
        for (slot, segment) in self.segments.iter_mut().enumerate() {
            if !segment.active {
                continue;
            }
            let elapsed = frame.timer_ms.wrapping_sub(segment.spawn_ms);
            if elapsed >= duration {
                segment.active = false;
            } else {
                let progress = ((elapsed as u64 * 255) / duration as u64) as u8;
                fade[slot] = 255 - progress;
            }
        }

        for (led_idx, pixel) in out.iter_mut().take(layout.count()).enumerate() {
            let (x, y) = layout.position(led_idx);
            let mut strongest = 0;
            let mut palette_pos = 0;
            for (slot, segment) in self.segments.iter().enumerate() {
                if fade[slot] == 0 {
                    continue;
                }
                let shape = segment_value(*segment, x, y);
                let value = scale255(shape, fade[slot]);
                if value > strongest {
                    strongest = value;
                    palette_pos = segment.palette_pos;
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

fn segment_value(segment: Segment, x: u8, y: u8) -> u8 {
    let to_node = point_coverage(x, y, segment.to_x, segment.to_y, NODE_RADIUS);
    let from_node = point_coverage(x, y, segment.from_x, segment.from_y, NODE_RADIUS);
    let line = scale255(
        segment_coverage(
            x,
            y,
            segment.from_x,
            segment.from_y,
            segment.to_x,
            segment.to_y,
            LINE_HALF_WIDTH,
        ),
        184,
    );
    to_node.max(from_node).max(line)
}

/// Coverage around a line segment, calculated in the same half-coordinate
/// space used by the radial PaletteFx effects. The projection test prevents
/// the line from extending beyond either constellation node.
fn segment_coverage(x: u8, y: u8, ax: u8, ay: u8, bx: u8, by: u8, width: u8) -> u8 {
    let (px, py) = (x as i16 / 2, y as i16 / 2);
    let (ax, ay) = (ax as i16 / 2, ay as i16 / 2);
    let (bx, by) = (bx as i16 / 2, by as i16 / 2);
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len_sq = (vx * vx + vy * vy) as u16;
    if len_sq == 0 {
        return point_coverage_half(px, py, ax, ay, width.max(1));
    }
    let dot = wx * vx + wy * vy;
    if dot <= 0 {
        return point_coverage_half(px, py, ax, ay, width);
    }
    if dot as u16 >= len_sq {
        return point_coverage_half(px, py, bx, by, width);
    }
    let length = sqrt16(len_sq).max(1) as i32;
    let cross = (wx as i32 * vy as i32 - wy as i32 * vx as i32).unsigned_abs();
    soft_edge((cross / length as u32).min(u8::MAX as u32) as u8, width)
}

fn point_coverage(x: u8, y: u8, cx: u8, cy: u8, radius: u8) -> u8 {
    point_coverage_half(
        x as i16 / 2,
        y as i16 / 2,
        cx as i16 / 2,
        cy as i16 / 2,
        radius,
    )
}

fn point_coverage_half(x: i16, y: i16, cx: i16, cy: i16, radius: u8) -> u8 {
    let dx = x.abs_diff(cx);
    let dy = y.abs_diff(cy);
    let distance = sqrt16(dx * dx + dy * dy);
    soft_edge(distance, radius)
}

fn soft_edge(distance: u8, radius: u8) -> u8 {
    if distance > radius {
        0
    } else {
        255 - (distance as u16 * 255 / (radius as u16 + 1)) as u8
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

    const POSITIONS: &[(u8, u8)] = &[(0, 128), (128, 128), (255, 128), (128, 220)];

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
    fn consecutive_keys_are_connected_without_lighting_off_path_keys() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = TracerState::<4>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(0, 128, 0);
        state.record_hit(255, 128, 10);
        state.tick(&layout, frame(10), &mut out);

        assert_ne!(out[0].v, 0);
        assert_ne!(out[1].v, 0);
        assert_ne!(out[2].v, 0);
        assert_eq!(out[3].v, 0);
    }

    #[test]
    fn a_long_pause_starts_a_new_constellation() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = TracerState::<4>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(0, 128, 0);
        let second = MAX_CONNECT_GAP_MS + 1;
        state.record_hit(255, 128, second);
        state.tick(&layout, frame(second), &mut out);

        assert_eq!(out[1].v, 0);
        assert_ne!(out[2].v, 0);
    }

    #[test]
    fn expired_segments_are_transparent() {
        let layout = SliceLayout::new(POSITIONS);
        let mut state = TracerState::<1>::new();
        let mut out = [Hsv::default(); 4];
        state.record_hit(0, 128, 0);
        state.tick(&layout, frame(duration_ms(128)), &mut out);
        assert!(out.iter().all(|pixel| *pixel == Hsv::default()));
    }
}
