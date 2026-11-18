# inv2sc — Inventor to SolidCraft

Turns Autodesk Inventor parts and assemblies into clean, editable [SolidCraft](https://github.com/tangency-labs/solidcraft) assemblies:

- **One SolidCraft design per part**, linked into the assembly. A part placed 40 times is one file, as it is in Inventor, and identical parts saved as separate files (`Bracket`, `Bracket_2`) share one.
- **Sheet metal stays sheet metal.** Inventor's thickness, bend radius and K-factor become a SolidCraft sheet metal rule, and each part is converted with `sheet.convert` at its real bends. It unfolds, gives a flat pattern and DXF, and takes new flanges. Parts that came from a plain STEP are recognised from their faces.
- **Constraints carry over.** Inventor mate, flush, insert, angle and tangent constraints become SolidCraft assembly constraints. Inventor joints (rigid, rotational, slider, cylindrical, planar, ball) become joints. For parts Inventor left unconstrained, joints are inferred from the geometry: shared hole and pin axes, fasteners, spacers and linkages. Travel limits come from where parts meet (`joint.find_limits`).
- **Repeats, patterns and symmetry are recognised.** Placements in a row, a grid or round an axis become a pattern with its spacing as a parameter. Mirror planes and mirrored part pairs are found and recorded on the design.
- **A report** (`conversion-report.md`) says what was found, what was decided and what SolidCraft did with every step.

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

`inv2sc analyze <input>` prints the analysis without SolidCraft. `inv2sc convert <input> --dry-run` writes the report and `build-steps.json` without building. `inv2sc watch <folder>` converts each export that appears in a folder: the exporter writes a `ready` file last.

### Without inv2sc

`inventor\Run-BuildSteps.ps1 -Steps build-steps.json` runs a `build-steps.json` with only PowerShell and SolidCraft. Use it where inv2sc isn't built, or with `inv2sc convert --dry-run --path-map FROM=TO` steps made on another machine.

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
- Inferred joints are a best guess from the geometry. The report lists each one with its reason. For assemblies that matter, constrain them in Inventor and the constraints carry over exactly.
- Mirror symmetry is recorded, not enforced: SolidCraft has no mirrored-component command yet.

## Licence

MIT or Apache-2.0, at your option, as SolidCraft. inv2sc is an independent tool and is not part of the SolidCraft project.
