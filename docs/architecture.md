# Architecture

## Why this shape

Three constraints set the design:

1. **SolidCraft changes fast.** Its design file format moves with most releases, and its plugin interface is `0.x` ("a minor version may break the one before it"). Its command surface is what its UI, MCP server, control channel and plugins all share, and every command has a catalogue entry and, increasingly, a JSON Schema. So inv2sc talks to SolidCraft only through commands, over MCP, with every name held in a data file.
2. **Inventor's API is COM**, and Laser Cut Workbench already drives it from PowerShell out of process. The exporter does the same: no add-in to install or compile, and the logic stays in Rust.
3. **Laser Cut Workbench will grow into this.** The neutral bundle and the analysis crates are meant to be its core, so they know nothing about SolidCraft.

## Stages

### 1. Export (Inventor, PowerShell)

`inventor/Export-InventorBundle.ps1` walks the assembly once and writes the bundle:

- each part document once (keyed by file and model state), with a STEP in its own frame (reused when it is newer than its `.ipt`), its iProperties, its mass properties and, for sheet metal, its style (thickness, bend radius, gap and reliefs), its unfold rule (K-factor when linear), a point on each bend face, the base face, and the flat pattern (DXF plus bend table: line, angle, direction, inner radius);
- each assembly: occurrences with placements, grounding, visibility and pattern membership; constraints with each side's geometry in the assembly's coordinates (plane point and normal, axis, cylinder radius and a rim point, solution type, offset, angle); Inventor joints with origins and limits; occurrence patterns.

Missing flat patterns are made inside a transaction that is aborted, so the source files are never changed.

### 2. Model (`lcw-analyze::model`)

A bundle or a STEP file becomes one `Model`: definitions (parts, assemblies) and their occurrences. STEP products with children are assemblies; a product with both children and bodies gets its bodies as an extra part.

### 3. Analysis

- **Features** (`features.rs`): round faces (holes and pins, with axial extent and sweep), flat faces with outward normals, a fingerprint.
- **Copies** (`repeats.rs`): parts whose vertex sets map onto each other by a rigid move (or only by a reflection: mirror images), found by registering principal frames. Same-name copies are merged by default.
- **Sheet metal** (`sheet.rs`): Inventor's data when present. Otherwise a plate is two large parallel faces a thickness apart, and a bend is a pair of coaxial cylinders whose radii differ by the thickness and that run along the sheet. The output is what `sheet.convert` needs: a point on the fixed face and a point on each bend.
- **Patterns** (`patterns.rs`): placements of one part in one assembly related by whole multiples of one translation (row), two (grid) or one turn about a common axis (circular).
- **Symmetry** (`symmetry.rs`): planes that map placed parts onto each other.
- **Joints** (`joints.rs`): Inventor constraints and joints carried over first. For parts with none: pins (coaxial hole/pin pairs) → fasteners go with their host → round parts go with their host → groups pinned on two or more axes are rigid → a group pinned on one axis turns if it hangs off the structure and its name says it moves (lever, arm, knob, wheel…); anything else on one pin is held to what it rides on → groups reached from the ground through pins get revolute joints, the rest are held rigid.

### 4. Build steps (`lcw-solidcraft::emit`)

Per part: new design, `file.insert_step`, `sheet.manage_rules` + `sheet.convert`, metadata, save. Then the assembly: sub-assemblies as nested components (`component.create`, then `occurrence.copy` for repeats), parts as linked components (`component.insert_design` once, then `occurrence.copy`), placement (`occurrence.move to_matrix`), pattern parameters (`parameters.change`, then `occurrence.move translate` by expressions), grounding, rigid groups, joints (`joint.create` with the same world frame on both sides and `flip: true`), constraints (`assembly.constraint.create`, falling back to the joint each rule stands for), limits (`joint.find_limits` → `joint.limits`), metadata, save.

Steps refer to SolidCraft's ids through bindings (`"${occ:3.1}"`), filled in as they run.

### 5. Run (`lcw-solidcraft::run`, `mcp`)

`solidcraft-cli mcp --headless` (or `--connect HOST:PORT` to the running app) is started with the output folder allowed. The runner probes the catalogue, checks the profile, then runs each design's steps. A failed soft step is recorded and the build goes on. A failed hard step stops that design but still saves it.

## The profile

`profiles/solidcraft.json` maps each operation (`insert_step`, `sheet_convert`, `joint`, …) to a command and its parameter names. The probe compares it with the running SolidCraft's catalogue and schemas:

- a command that is gone disables the op (and its fallback is used, or the step is skipped and reported);
- a parameter the schema no longer lists is reported, so the profile can be fixed before a build goes wrong.

## What is not used, and why

- **Writing `.sc` files directly**: the fastest path, but it breaks with every format bump.
- **An importer plugin (`file.import`)**: SolidCraft's importer world writes a design file, so it has the same problem. It also has no access to commands.
- **A command plugin**: it does have `run(command, args)`, the same command surface as the runner. Once SolidCraft's plugin API is stable, a thin plugin can wrap `lcw-analyze` + `lcw-solidcraft` (they are dependency-free and build for wasm) to give an *Import Inventor bundle* menu item inside SolidCraft.
