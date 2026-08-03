# rmk-palettefx

A `no_std` Rust port of Pascal Getreuer's QMK
[PaletteFx](https://getreuer.info/posts/keyboards/palettefx) community module
for [RMK](https://rmk.rs/) keyboard firmware.

Effects write HSV values into a caller-supplied frame buffer. The
caller converts to RGB and ships the result to whatever LED driver
they have wired up. This crate is MCU, HAL, and driver agnostic.

## Effects

- Gradient
- Flow
- Vortex
- Sparkle
- Ripple
- Rain
- Reactive
- Crosshair

### Crosshair

Crosshair reacts to a key press across that key's physical row and column.
The exact pressed LED uses `Key hue`; the arms use `Arm hue`. Its runtime
parameters are:

- `Motion`: `0` flash, `1` inward streaks, `2` outward streaks, `3` collapse
- `Duration x10ms`: base lifetime; the shared speed control makes it shorter
  or longer (`128` preserves the configured duration)
- `Arm width`: row/column coordinate tolerance
- `Pulse width`: moving-streak width and collapse-edge softness
- `Crosses`: newest crosshairs allowed at once; `1` makes every new press
  immediately replace the previous animation
- `Arm hue` and `Key hue`: independent HSV hues

These starting points exercise the main variants:

| Preset | Motion | Duration | Arm width | Pulse width | Crosses |
| --- | ---: | ---: | ---: | ---: | ---: |
| Instant singleton | 0 | 12 | 8 | 56 | 1 |
| Fast converge | 1 | 16 | 8 | 56 | 4 |
| Converge singleton | 1 | 16 | 8 | 56 | 1 |
| Fast outward | 2 | 16 | 8 | 56 | 4 |
| Quick collapse | 3 | 14 | 8 | 40 | 4 |

## Reference consumer

[rmk-zsa-voyager](https://github.com/jpds/rmk-zsa-voyager) drives the
ZSA Voyager's 52-key per-key RGB through this crate. See its codebase for an
end-to-end consumer.

## Credits

All palette data and effect algorithms come from Pascal Getreuer's
[QMK PaletteFx module](https://github.com/getreuer/qmk-modules/tree/main/palettefx).

## License

[Apache-2.0](LICENSE).
