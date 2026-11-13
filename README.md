# lumen

Lighting, fan and AIO-screen control for an Omarchy desktop. It replaces OpenRGB here.

| Device | What lumen does |
|---|---|
| Corsair iCUE LINK System Hub | Per-LED lighting on all fans and the pump head; fan and pump curves; sensor readout |
| iCUE LINK AIO LCD (TITAN 360) | 480×480 screen with named, configurable screens: graphs, bars and rings of any metrics, plus a coolant gauge, a pump-and-fans view and a clock. It can rotate through screens, show an image or GIF, or show the Corsair firmware screen. Any screen can sit on a background image, a GIF or your Omarchy wallpaper |
| Corsair K70 LUX / K70 LUX RGB | Per-key backlight. The model is detected automatically, and the keyboards can be swapped while the daemon runs. On the red-only K70 LUX, effects set brightness rather than color |
| Logitech G502 HERO | Logo and DPI-strip colors (HID++ 2.0, feature 0x8070) |
| Sensors | CPU temperature from k10temp/coretemp and CPU load from `/proc/stat`. GPU readings come from NVIDIA through NVML (loaded at runtime), otherwise from the amdgpu card with the most VRAM, so a discrete GPU wins over integrated graphics. Coolant temperature comes from the AIO |

## Pieces

- `lumend` runs as a systemd user service and owns all the devices. It listens for commands on `$XDG_RUNTIME_DIR/lumen.sock`.
- `lumenctl` is the CLI: `lumenctl status`, `lumenctl light hub:fans wave`, `lumenctl fan pump 30:60 40:85 45:100`, `lumenctl lcd cycle --cycle coolant,rings,graph --interval 10`
- **Bar widget** (`pxsean.lumen`, in its own repo: [omarchy-lumen](https://github.com/pxSean/omarchy-lumen)) sits in the right side of the Omarchy bar, shows a fan icon and the coolant temperature, and turns the urgent color during the coolant safety override. Left click opens a panel with temperatures, pump and fan speeds, and one-click buttons for profiles, AIO screen layouts and lighting presets. Right click opens the TUI.
- `lumen` is the TUI. Launch it as **Lumen** from the app launcher; it opens as a floating Omarchy terminal.

Settings are saved to `~/.config/lumen/config.toml` on every change. Colors can be `#rrggbb`, `theme` (the active Omarchy theme's `keyboard.rgb`), or `theme:<name>` (any key in the theme's `colors.toml`). When you run `omarchy theme set`, the daemon picks up the new theme within a couple of seconds.

### Lighting targets

The most specific target wins:

1. `hub:ch3`, `k70:keys`, `g502:logo`
2. `hub:aio` or `hub:fans`
3. `hub`, `k70` or `g502`
4. `*`

### Fan targets

The most specific target wins: `ch<N>`, then `pump` or `fans`.

## Safety

- The pump never runs below 50%. The hub driver enforces this.
- If coolant reaches 55 °C, every fan and the pump go to 100% until it drops below 50 °C.
- If a curve's temperature sensor can't be read, that curve runs at 100%.
- When the daemon stops (stop, crash or unwind), the hub goes back to its onboard fan curve, and the LCD, keyboard and mouse go back to their firmware lighting.

## LCD screens

A *screen* is a style plus its options:

| Style | Options |
|---|---|
| `graph` | 1–4 metrics and a plot size. Metrics that share a unit share an auto-fitted axis; with mixed units, each line is scaled to that metric's own range |
| `bars` | 1–4 metrics, one row each |
| `rings` | An outer and an inner gauge metric, plus an optional footer metric |
| `coolant`, `cooling`, `clock` | None |

The metrics are:

- **CPU:** `cpu.temp`, `cpu.load`
- **Discrete GPU:** `gpu.temp`, `gpu.load`, `gpu.power`, `gpu.vram`
- **Integrated GPU:** `igpu.temp`, `igpu.load`
- **Memory:** `mem.used`
- **Cooling:** `coolant.temp`, `pump.rpm`, `pump.duty`, `fans.rpm`, `fans.duty`

`lumenctl metrics` lists them all with their current values.

```
lumenctl screen                                   # list screens (● showing, ↻ in cycle)
lumenctl screen gpus --style graph --metrics gpu.temp,igpu.temp,cpu.temp --scale 1.2
lumenctl screen rings --outer cpu.load --inner gpu.load --footer mem.used
lumenctl screen gpus --rename temps    |    lumenctl screen temps --delete
lumenctl lcd gpus                                 # show it; or: lumenctl lcd cycle --cycle coolant,gpus,load
```

In the TUI's Screen tab:

- `e` edits the selected screen: ←→ changes the style, size or gauge metric, and space toggles a metric on or off.
- `n` creates a new screen, `R` renames the selected one, and `x` deletes it.

Configs written before screens existed (with `mode =` and `graph_scale =`) migrate automatically.

## LCD backgrounds

`lumenctl lcd --background wallpaper` puts the current Omarchy wallpaper behind every drawn layout. It follows `omarchy theme bg next` and theme changes. You can also pass a path to a PNG, JPEG, WebP or GIF instead of `wallpaper`. `--dim 0-100` (default 55) blends the background toward the theme background color, and `--no-background` removes it. Images are center-cropped to the 480×480 round panel.

Animated GIF backgrounds redraw the layout on every frame, at up to 20fps; this measured about 5% of one CPU core. GIFs longer than 150 frames are thinned to 150 to limit memory.

## LCD previews

`lumend --preview DIR` renders every layout to PNG using the current theme and sample data, without touching the device.

## Install

```
./install.sh   # release build → ~/.local/bin, systemd user unit, launcher entry, udev rule
```

Then add the bar widget:

```
omarchy plugin add https://github.com/pxSean/omarchy-lumen.git --enable
```

## Troubleshooting

If the LCD panel stops responding, first try a soft power cycle of the panel:

```
systemctl --user stop lumend
cargo run -p lumen-hid --example lcd_power_cycle
systemctl --user start lumend
```

If that doesn't bring it back, the panel's controller is probably wedged. Its firmware keeps standby power, so a reboot won't reset it. Use Corsair's recommended fix: shut the machine down, unplug the power cable, hold the power button for at least 30 seconds to drain residual power, then plug it back in and boot.

## Credits

The protocols come from [OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub) (GPL-3.0) and [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) (GPL-2.0+). lumen is GPL-3.0-or-later.
