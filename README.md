# inv2sc — Inventor to SolidCraft

Turns Autodesk Inventor parts and assemblies into clean, editable [SolidCraft](https://github.com/tangency-labs/solidcraft) assemblies:

- **One SolidCraft design per part**, linked into the assembly. A part placed 40 times is one file, as it is in Inventor, and identical parts saved as separate files (`Bracket`, `Bracket_2`) share one.
- **Sheet metal stays sheet metal.** Inventor's thickness, bend radius and K-factor become a SolidCraft sheet metal rule, and each part is converted with `sheet.convert` at its real bends. It unfolds, gives a flat pattern and DXF, and takes new flanges. Parts that came from a plain STEP are recognised from their faces.
- **Joints come from the geometry, checked against Inventor.** For every assembly, whatever its source, pivots, rigid groups, fasteners, spacers and linkages are read off the parts' real geometry and placements: shared hole and pin axes, fasteners, round parts, parts pinned on two axes. Inventor's constraints are evidence: which parts the designer related and how (an insert confirms a pin, and a looser fit is tried where it found none; an angle constraint marks a part that turns; a mate or flush decides what a part with no pin is held to). An Inventor constraint becomes a SolidCraft constraint only when its exported geometry is complete and lands on the parts' faces; nothing is ever made from a guess that could lock or free parts. Inventor joints with an exported origin become joints. Travel limits come from where parts meet (`joint.find_limits`).
- **The mechanism is checked in SolidCraft.** After the assembly is saved, `joint.list` gives the degrees of freedom, conflicting joints and floating chains, and each joint is driven a little and back (`joint.drive`). The report lists every joint with what it came from (geometry, Inventor, both), whether it was made, its limits and whether it moves.
- **Repeats, patterns and symmetry are recognised.** Placements in a row, a grid or round an axis become a pattern with its spacing as a parameter. Mirror planes and mirrored part pairs are found and recorded on the design.
- **A report** (`conversion-report.md`) says what was planned and, separately, what SolidCraft actually built: per design and per step group, every failure with SolidCraft's own error, every step left out and why, and where the time went.

It is built to keep working as SolidCraft changes, which it does quickly. See [Staying compatible](#staying-compatible).

## How it fits together

```
Inventor ──Export-InventorBundle.ps1──▶ bundle (bundle.json + one STEP per part + flat DXFs)
                                                │
               any STEP assembly ───────────────┤
                                                ▼
                              inv2sc: analyse (Rust, no dependencies)
                     copies · sheet metal · patterns · symmetry · joints
                                                │
                                 build steps (build-steps.json)
                                                │
                       SolidCraft MCP server (solidcraft-cli mcp)
                        headless, or --connect to the running app
                                                ▼
                     parts/*.sc  +  <assembly>.sc  +  conversion-report.md
```

| Crate | What it does |
|---|---|
| `lcw-json` | Dependency-free JSON |
| `lcw-geom` | Vectors, placements, axes, point-set registration |
| `lcw-step` | STEP reader: product tree, placements, B-rep summary; writes single parts out |
| `lcw-bundle` | The versioned interchange format (`lcw-bundle/1`) |
| `lcw-analyze` | Copies, sheet metal, patterns, symmetry, joints |
| `lcw-solidcraft` | Command profile, build steps, MCP client, step runner |
| `lcw-bridge` | The `inv2sc` program |

The crates are prefixed `lcw-` because they are meant to become the core of Laser Cut Workbench as well. See [docs/laser-cut-workbench.md](docs/laser-cut-workbench.md).

## Quick start (Windows)

1. **Get `inv2sc.exe`.** Download it from the latest CI run (*Actions › CI › inv2sc-windows*), or build it with `cargo build --release` (it needs [Rust](https://rustup.rs) and nothing else, not even internet access after installing Rust).
2. Put `inv2sc.exe` in the `inventor` folder next to `Export-InventorBundle.ps1`.
3. Open the assembly in Inventor and double-click **`Send to SolidCraft.cmd`**.

Or step by step:

```bat
powershell -STA -ExecutionPolicy Bypass -File inventor\Export-InventorBundle.ps1 -OutFolder C:\exports\console
inv2sc convert C:\exports\console
```

From a plain STEP assembly, with no Inventor:

```bat
inv2sc convert "console and base assembly.stp"
```

To watch the result being built in the open SolidCraft window, turn on the control channel there (or start it with `solidcraft --control 7878`) and add `--connect 127.0.0.1:7878`.

While it builds, inv2sc prints a line per step (at most a few a second) and keeps `progress.json` (where it is) and `build-log.jsonl` (every step as it finishes, with its time and SolidCraft's answer) in the output folder. A SolidCraft call that doesn't answer in time (`--timeout`, default 300 s, longer for the slow commands) fails its step, and the next design starts in a fresh SolidCraft session. `--batch 50` sends independent steps 50 at a time. `--relative-paths` makes `build-steps.json` movable: `inv2sc run <folder>\build-steps.json` builds it where it is.

`inv2sc analyze <input>` prints the analysis without SolidCraft. `inv2sc convert <input> --dry-run` writes the report and `build-steps.json` without building. `inv2sc watch <folder>` converts each export that appears in a folder: the exporter writes a `ready` file last.

### Without inv2sc

`inventor\Run-BuildSteps.ps1 -Steps build-steps.json` runs a `build-steps.json` with only PowerShell and SolidCraft. Use it where inv2sc isn't built, or with `inv2sc convert --dry-run --path-map FROM=TO` steps made on another machine.

## Verifying a build

`inv2sc verify <bundle> --built <folder convert wrote>` checks a finished build against the bundle it came from, independently of the converter: it asks SolidCraft what it holds and compares that with what the bundle says Inventor had.

1. **Part designs.** Every part document with a STEP has a saved design; its volume and bounding box match SolidCraft's own import of that STEP (0.1 % and 0.05 mm), and Inventor's volume and range box when the bundle carries them (0.5 % and 1 mm or 1 % of the diagonal: the range box is not tight on curved parts). A volume out by the same factor on every part is reported as a units problem.
2. **Placements.** Every placed occurrence, through sub-assemblies to any depth, is present exactly once, in the sub-assembly Inventor has it in, at its position and orientation. The expected world placement is the product of the bundle's matrices down the path, computed by the checker. Each built instance is measured in the world by SolidCraft (centre of mass, inertia, box) and judged against the part's own STEP placed by that matrix, besides its frame being compared; a frame that differs while the geometry coincides (a symmetric part) is listed, not failed. Missing, extra and misplaced ones are reported with the distance and angle.
3. **Names.** Occurrences and designs carry the Inventor names.
4. **Overall.** Part count, and the world bounding box against the one the bundle's placements give.
5. **Interference.** SolidCraft's own check on the closed bodies: information, since the source may clash itself. A pair is suspect when a large part of the smaller body is inside the other and one of the two is not where Inventor has it.

It writes `verification-report.md` and `verification.json` (to `--out`, default the built folder), prints one summary line and exits 1 when a check fails. Every SolidCraft command, tool and answer key it uses is in `profiles/solidcraft.json` (`verify_*` ops and the `verify` section). A full check of a 190-part assembly takes about 15 minutes, most of it the interference check (`--clash-budget`, `--no-clash`).

## Staying compatible

SolidCraft is young and moves fast. Its own contributor rules say a command may change, and its plugin API is 0.x. inv2sc depends on as little as it can:

1. **It drives SolidCraft only through its supported, documented interfaces:** the MCP server and the command catalogue, the same commands the UI runs. It never writes SolidCraft's design file format, which changes with almost every release.
2. **No command or parameter name is in the code.** Every operation goes through [`profiles/solidcraft.json`](profiles/solidcraft.json). When an update renames something, edit the profile (or pass `--profile`); nothing is recompiled.
3. **Every run starts with a probe.** inv2sc reads SolidCraft's catalogue and JSON Schemas (`list_commands`, `describe_command`) and checks every operation in the profile. A missing command is switched to its fallback, or skipped and reported, instead of failing half way. `inv2sc probe` runs the check alone.
4. **Every risky step has a fallback.** For example, a constraint SolidCraft refuses becomes the joint it stands for. A part that won't convert to sheet metal stays a solid and is listed in the report.
5. **The bundle format is versioned and only grows**, so exports from older exporters keep working.

## Building

```sh
cargo build --release        # target/release/inv2sc(.exe)
cargo test                   # unit tests and an end-to-end test against a recording SolidCraft
python3 tools/make_fixtures.py   # regenerate the test STEP files
```

No dependencies outside the Rust standard library: it builds offline, and the core crates also build for WebAssembly, so they can run inside a SolidCraft plugin.

## Limits

- Inventor's feature history (extrudes, holes, fillets) does not come across: parts are exact solids, and sheet metal parts are native SolidCraft sheet metal built from those solids. Rebuilding flanges as separate flange features from Inventor's flat pattern is planned (the bundle already carries the flat pattern and bend table).
- Inferred joints are a best guess from the geometry. The report lists each one with its reason and the Inventor constraints that agree with it. Inventor constraints carry over as SolidCraft constraints only when the exporter records their geometry completely (see `docs/architecture.md`).
- Mirror symmetry is recorded, not enforced: SolidCraft has no mirrored-component command yet.

## Licence

MIT or Apache-2.0, at your option, as SolidCraft. inv2sc is an independent tool and is not part of the SolidCraft project.
