# stormworks-infrared

A plugin for [Stormworks: Build and Rescue](https://store.steampowered.com/app/573090/Stormworks_Build_and_Rescue/) that exposes the game's own heat and temperature system to component-mod Lua scripts. It adds a global `ir` table, so custom components can build real infrared seekers, heat detectors and homing guidance.

## READ THIS FIRST

stormworks-infrared uses low-level techniques (DLL injection + function hooking) to extend the game, because there is no supported way to do it.

- No guarantees about stability. Every function the plugin hooks is verified against a known byte pattern first, and if it doesn't match (usually after a game update) that hook is skipped and the plugin says so in its log instead of patching blindly — but injection itself can still be a bad time.
- **Antivirus software might flag it.** Injecting into another process is exactly what malware does, so this is expected. If you want to be sure what it does, read the source and build it yourself.
- It does not modify any game files on disk.

## Download

Latest release: https://github.com/dongfengmissile/stormworks_infrared/releases/latest

## Build

Requires the Rust toolchain and the GNU Windows cross target:

```
rustup target add x86_64-pc-windows-gnu
cargo build --target x86_64-pc-windows-gnu
```

or for the minimal logging release:

```
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Usage

1. Start Stormworks and load into a world.
2. Inject `StormworksInfrared.dll` into `stormworks64.exe` — either with the bundled `infrared_inject.exe` or any other DLL injector.

On Linux, the game runs under Proton, so the injector has to run inside the same Proton prefix Steam created for Stormworks (573090) rather than a bare WINE prefix.

You should see a line in the log confirming the hooks installed and the `ir` table was registered. Logs go to:

```
<Stormworks install>/logs/infrared_bootstrap.log
```

The debug build additionally carries a diagnostic dump: weather, sun, scan direction, source list, source position and more.

## Lua API

The plugin only provides the Lua side; the components that call it are distributed separately.

```lua
-- Temperature in Celsius at a world point.
ir.probe(x, y, z)

-- Staring infrared array scan.
ir.scan(az_steer, el_steer, fov, range, rx, ry, gain, detect, axis, occlusion, pack, cells_out)

-- A seeker's last scan, for a microcontroller (drawing) script.
ir.frame(id)

-- FLIR presentation of the camera feeding this microcontroller's video input
-- (microcontroller scripts only).
ir.flir_config(r, g, b, black_hot, tmin, tmax)
```

| argument | meaning |
|---|---|
| `az_steer`, `el_steer` | boresight steering, in turns (1.0 = 360°) |
| `fov` | full cone field of view, in turns |
| `range` | metres (sources beyond this are culled) |
| `rx`, `ry` | cell grid, `rx*ry` cells |
| `gain` | IR gain — the flux that maps to full-scale white |
| `detect` | detect threshold in raw flux (the component applies it) |
| `axis` | component-local forward axis: `0=+X 1=-X 2=+Y 3=-Y 4=+Z 5=-Z` |
| `occlusion` | `1` = use occlusion, `0` = off |
| `pack` | pack 2 cells per float (12-bit each) when the grid exceeds one composite (33–64 cells) |
| `cells_out` | `0` returns an empty cell table — for a seeker whose image only reaches a display through `ir.frame`, which avoids 4096 Lua table insertions per scan at 64×64 |

Returns:

| value | meaning |
|---|---|
| `cells` | Lua array: `rx*ry` numbers `0..1`, or `ceil(rx*ry/2)` packed floats (empty when `cells_out` is `0`) |
| `az_err`, `el_err` | direction of the target, in turns (greatest signal inside the acquisition cone) |
| `strength` | `0..1` |
| `total_flux` | raw IR power at the array — **this is the detection signal** |
| `base_temp` | ambient temperature at the seeker |
| `n_sources` | how many sources survived culling and occlusion |
| `id` | this seeker's identity (24 bits, never 0, exact in an f32) — pass it to `ir.frame(id)` |

Two things worth knowing when tuning:

- **A narrower FOV does not increase detection range.** `total_flux` is conserved no matter how the scene maps onto the cells, so FOV only buys angular precision (`fov/cells`) and a tighter acquisition cone.
- **Detection is a threshold on `total_flux`,** so lowering `detect` extends range roughly as `sqrt(1/threshold)`.

A minimal component would look like:

```lua
local cells, az_err, el_err, strength, flux, ambient, n_sources, id = ir.scan(
    az, el, 0.0125, 1000, 4, 4, 0.005, 0.001, 2, 1, false)
```

## FLIR camera view

A microcontroller whose **video input** is wired to a camera can turn FLIR on for that camera. The camera's own **Infrared Mode** is left alone and keeps working as the game renders it. Every vehicle block carries a simulated temperature (mass-weighted heat capacity, conduction between neighbouring blocks, the game's own engine/fire/fluid heat as a source, sun and air), so a worn engine shows hot while a cold hull stays near ambient. Terrain and water sit at ambient plus sun soak; sky is cold.

```lua
ir.flir(on)                                      -- bool (or number, nonzero = on)
ir.flir_config(r, g, b, black_hot, tmin, tmax)   -- presentation only
```

`ir.flir(on)` enables or disables FLIR for the camera feeding this microcontroller. **Call it every tick:** the enable expires 1 s after the last call, so FLIR switches itself off if the script stops or the vehicle is removed. `ir.flir(false)` disables immediately. Returns `true` if a camera was resolved from the video link, `false` otherwise (use it to show "no camera").

`ir.flir_config` sets the presentation and does not enable FLIR:

| argument | meaning |
|---|---|
| `r`, `g`, `b` | tint multiplied into the image, clamped to `0..1` (default `1, 1, 1`) |
| `black_hot` | `true` = black hot (the tinted white-hot picture with its colours inverted, like an image editor's invert), `false` = white hot (default white hot) |
| `tmin`, `tmax` | display window in °C, `tmin < tmax` (default `-5`, `60`) |

It returns `true` when stored, `false` when arguments are invalid or the video input is not fed by a camera. Without a config call, defaults are used.

Wiring:

1. Put a camera and a microcontroller on the vehicle.
2. Wire the camera's **video output** to the microcontroller's **video input**.
3. Wire the microcontroller's **video output** to the monitor; the FLIR image is shown there with the script's `onDraw` overlay on top.
4. Call `ir.flir` (and optionally `ir.flir_config`) from `onTick`. The link is resolved per call; state is keyed by the camera, so two cameras need two microcontrollers.

See `examples/ir_flir.lua`. In a development (debug) build, **F10** toggles FLIR on the main player view as well, for tuning without a camera.

## What the seeker sees

- **Heat sources** — every engine type, fluid pipes, anything the game tracks heat for.
- **Fires and flares** — a lit flare registers as a fire, so it works as an IR countermeasure once it is inside the acquisition cone and outshines the target
- **The sun** — blocked by terrain; inside the acquisition cone it outshines any target and steals the lock
- **Smoke** — cool: only burning fires (and flares) are hot, not smoke or a burnt-out fire's lingering particles
- **Weather** — fog and rain attenuate every emitter, fog far more than rain
- **Contrast** — a seeker sees radiance above the background (the air at the seeker), so anything at or below air temperature is invisible
- **Water** — opaque to IR: nothing underwater is seen, and a seeker underwater sees nothing
- **Its own vehicle** — every heat source on the seeker's own vehicle is ignored
- **Occlusion** — players, terrain, buildings and vehicle hulls block sources via the game's raycast

## Game updates

The plugin checks each function it hooks before touching it. If a game update moves something, that hook is skipped and reported in the log rather than causing undefined behaviour, and the rest of the plugin keeps working on whatever is still valid.

## Disclaimer

I am not personally affiliated with the developers or publishers of Stormworks: Build and Rescue, nor have they endorsed this project. Stormworks: Build and Rescue and any of its content or materials are the property of their respective owners.
