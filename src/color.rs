//! Color types and HSV → RGB conversion.
//!
//! The HSV convention uses `h` sweeping a full turn over 0..=255
//! (so each 60° sector is 43 wide), `s`/`v` are ordinary 0..=255 fractions.

/// 8-bit HSV triple.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Hsv {
    pub h: u8,
    pub s: u8,
    pub v: u8,
}

impl Hsv {
    pub const fn new(h: u8, s: u8, v: u8) -> Self {
        Self { h, s, v }
    }
}

/// 8-bit RGB triple.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Alpha-compose `overlay` over `base` with straight 0..=255 coverage.
///
/// The endpoints are exact: zero returns `base`, and 255 returns `overlay`.
pub const fn blend_rgb(base: Rgb, overlay: Rgb, alpha: u8) -> Rgb {
    const fn channel(base: u8, overlay: u8, alpha: u8) -> u8 {
        let alpha = alpha as u32;
        (((base as u32) * (255 - alpha) + (overlay as u32) * alpha + 127) / 255) as u8
    }

    Rgb::new(
        channel(base.r, overlay.r, alpha),
        channel(base.g, overlay.g, alpha),
        channel(base.b, overlay.b, alpha),
    )
}

/// Integer HSV → RGB using the six-sector spectrum (same shape as the
/// `smart-leds` implementation). `h` partitions into 43-wide sectors; inside
/// each sector one channel holds at `v`, one at `p`, and the third ramps.
pub fn hsv_to_rgb(hsv: Hsv) -> Rgb {
    let h = hsv.h as u32;
    let s = hsv.s as u32;
    let v = hsv.v as u32;

    let sector = h / 43;
    let remainder = (h - sector * 43) * 6; // 0..=240 inside each sector

    let p = (v * (255 - s)) >> 8;
    let q = (v * (255 - ((s * remainder) >> 8))) >> 8;
    let t = (v * (255 - ((s * (255 - remainder)) >> 8))) >> 8;

    match sector {
        0 => Rgb::new(v as u8, t as u8, p as u8),
        1 => Rgb::new(q as u8, v as u8, p as u8),
        2 => Rgb::new(p as u8, v as u8, t as u8),
        3 => Rgb::new(p as u8, q as u8, v as u8),
        4 => Rgb::new(t as u8, p as u8, v as u8),
        _ => Rgb::new(v as u8, p as u8, q as u8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_blend_preserves_exact_endpoints() {
        let base = Rgb::new(10, 20, 30);
        let overlay = Rgb::new(200, 150, 100);
        assert_eq!(blend_rgb(base, overlay, 0), base);
        assert_eq!(blend_rgb(base, overlay, 255), overlay);
    }
}
