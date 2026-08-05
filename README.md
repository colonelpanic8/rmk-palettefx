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
- Tracer
- Keyfall
- Shockwave
- Comet

### Typing-reactive effects

- **Tracer** connects consecutive presses with fading lines and bright nodes,
  leaving a short-lived constellation of the typing path. A pause starts a new
  constellation instead of drawing a long unrelated bridge.
- **Keyfall** launches a narrow palette-colored streak from each pressed key
  toward one board edge. Its single runtime parameter is:
  - `Gravity`: `0` down, `1` up, `2` left, `3` right. A streak already in
    flight keeps the direction it launched with, so changing this aims the
    next press rather than bending the current ones.
- **Shockwave** launches an expanding palette-colored ring from each pressed
  key.
- **Comet** flies a single body to each pressed key instead of lighting it
  where it was pressed. It arrives late, keeps the speed it had through a
  redirect so it swings wide of a key it is pulled onto mid-flight, and drags
  a short trail along the path it flew. Its runtime parameters are:
  - `Lag x10ms`: time for a flight the full width of the board, which is the
    lag at its worst. The shared speed control scales it (`128` preserves the
    configured value).
  - `Distance pace`: how strongly a hop's distance sets its flight time. `0`
    gives every flight the same time however far it is, so a hop to the next
    key over crawls while one across the board tears across it. `255` makes
    the time follow the square root of the distance: short hops finish
    quickly and long ones stay bounded by `Lag`. This is what keeps the comet
    in touch with fast typing -- each press restarts the flight, so a comet
    that always spent a full lag would drift after a target that keeps moving
    and fall further behind with every keystroke.
  - `Momentum`: how much of the in-flight velocity a redirect inherits, out
    of `128`. `0` stops the comet dead at every press; above `128` it gains
    speed through a turn and overshoots further.
  - `Trail x10ms`: how far back along the flown path the trail reaches. `0`
    leaves a bare head.
  - `Head size`: radius of the body in board units.
  - `Linger x10ms`: how long the comet takes to fade out once it has arrived
    and no further key has been pressed. A press after it has faded starts a
    new flight at that key rather than dragging the body in from where it
    died.

All four are sparse and can be selected either as the primary effect or as an
overlay. They use the shared palette, speed, and brightness controls.

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
