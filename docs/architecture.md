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

`solidcraft-cli mcp --headless` (or `--connect HOST:PORT` to the running app) is started with the output folder allowed. The runner probes the catalogue, checks the profile, then runs each design's steps. A failed soft step is recorded and the build goes on. A failed hard step stops that design but still saves it; the steps it leaves out are reported as *not run*, with the step that stopped them.

**As it goes.** Every step is timed and, as it finishes, appended and flushed to `build-log.jsonl` (JSON lines: `design`, `step` with status, SolidCraft's error text, command and milliseconds, `design_done`, `event`, `run_done`), so a hang or a crash keeps everything up to that step. `progress.json` (`lcw-progress/1`: phase, design, step of total, elapsed seconds, failures so far) is rewritten whole (written beside it, then renamed) for another program to show. Progress lines go to stderr, at most four a second, with failures and steps slower than 5 s always shown.

**Timeouts and the session.** Every call has a time limit: the profile's `runner.timeout_s` (300 s), an op's own `timeout_s` for the known slow ones (`find_limits` 900 s, `insert_step` and `sheet_convert` 600 s), a session tool's `runner.tool_timeout_s` (`save` 600 s); `--timeout` raises them all to at least its value. A headless server runs every call to its end, so a call that times out may still be running and would change whatever comes next: the client kills the server, the step fails with a message saying so, the rest of that design is reported as not run (it was not saved), and the next design starts in a new server. That is sound because each design is its own file and its ids are bound afresh (the runner forgets bound ids at each design); an assembly whose part was lost can't insert that part, a hard step, so the assembly stops there (saved as far as it got) and the report names the part. Connected to the app, the bridge answers a call that outlives its own wait (240 s) with `still running:`: the runner waits for it with `pending` up to the same limit, then asks `cancel`. Cancelled, the design is as it was and the build goes on; not cancellable, the session is given up as above. If SolidCraft can't be started again, the remaining designs are reported as not run, with the reason.

**Batches (`--batch N`).** SolidCraft's `batch` tool runs a list of `{command, params}` in order and stops at the first error. Its steps can't refer to each other's results, so the runner sends a run of consecutive op steps whose values are all known before the batch (none uses an id an earlier one in the batch binds), up to N, with `rollback: false` and `timings: true`. The results bind ids as single calls do; SolidCraft's per-step timings become the steps' times. When step *k* fails, steps before it are kept, step *k* gets its retries and alternatives alone (with the error the batch gave), and the run goes on from *k + 1*. A step refused before anything ran (SolidCraft checks a batch's parameters first, headless) is taken up one at a time. A batch failure that doesn't say which steps ran leaves the design's state unknown: the session is given up and batching is turned off for the rest of the run. Off by default; the one-at-a-time path is unchanged. Placements bind an id that the next step uses (insert, then move), so the placement phase batches in pairs at best; joints, constraints, grounding and properties batch in long runs. Whether this saves much depends on where the time goes: the build log and the report's *Where the time went* show time per command, which tells round-trip overhead from time spent inside SolidCraft.

**Paths.** `--relative-paths` writes every path under the output folder as `${steps_dir}/…`, resolved against the steps file's folder when the steps are run (`inv2sc run build-steps.json`), so the folder can be moved or copied to another machine. `--path-map` still rewrites absolute paths for another machine. `inventor/Run-BuildSteps.ps1` does not resolve `${steps_dir}` yet: it would need `Resolve-Vars` to replace a leading `${steps_dir}` by the steps file's folder.

The PowerShell runner does not have these yet: progress file and per-step log flushing (it writes `-Log` at the end), per-step timing, timeouts and session restarts, `pending`/`cancel`, `--batch`, `${steps_dir}` paths, reporting skipped and not-run steps with reasons, counting limit steps, and reporting a limit with one unknown end.

### 6. Verify (`lcw-verify`)

A separate crate that shares nothing of the converter's analysis or step building: only the bundle reader, the vector maths, the MCP client and the profile. `expect.rs` expands the bundle's occurrences into world placements; `obs.rs` reads SolidCraft's component tree (placements are row-vector matrices: translation in the last row, so they are transposed), links and per-instance measurements; `judge.rs` compares one instance with the part's own STEP placed by the bundle's matrix (centre of mass for position, the inertia tensor per unit mass for orientation, the box between the box of the transformed STEP vertices and of the transformed box corners); `assign.rs` pairs expected and built instances by minimum total cost, so a swapped pair of identical parts is not reported as two misplacements (names are judged separately); `verify.rs` makes the five checks. A scripted fake SolidCraft in `tests/` answers in the shapes the real one does.

Reference for the part check: the bundle's `mass` (Inventor's volume and range box) when the exporter wrote it, and always SolidCraft's own import (`open`) of the bundle's STEP, which tests everything the converter does to a part.

## The profile

`profiles/solidcraft.json` maps each operation (`insert_step`, `sheet_convert`, `joint`, …) to a command and its parameter names. The probe compares it with the running SolidCraft's catalogue and schemas:

- a command that is gone disables the op (and its fallback is used, or the step is skipped and reported);
- a parameter the schema no longer lists is reported, so the profile can be fixed before a build goes wrong.

## What is not used, and why

- **Writing `.sc` files directly**: the fastest path, but it breaks with every format bump.
- **An importer plugin (`file.import`)**: SolidCraft's importer world writes a design file, so it has the same problem. It also has no access to commands.
- **A command plugin**: it does have `run(command, args)`, the same command surface as the runner. Once SolidCraft's plugin API is stable, a thin plugin can wrap `lcw-analyze` + `lcw-solidcraft` (they are dependency-free and build for wasm) to give an *Import Inventor bundle* menu item inside SolidCraft.

## Known limits, from real runs

Run against a real King Air console assembly (78 parts, 11 sheet metal, 31 joints) in SolidCraft 0.44.1:

- All solid parts place and save. A part with surfaces but no solid body (a reference sketch exported as STEP) is left out and reported, rather than sent to `file.insert_step` to fail there.
- 30 of 31 pivots build, each with its travel limits found by `joint.find_limits` swept only against the parts near it (SolidCraft cheks at most 256 bodies in one contact sweep; sweeping the whole assembly for every pivot would exceed that on a large design). The one pivot that doesn't build is between two parts that are both inside the same copy of a repeated sub-assembly: SolidCraft anchors a joint's origins on the occurrence they were picked on, and two parts inside one copy resolve to that copy's own occurrence, which `joint.create` refuses (fine on the first copy, where the pick is also the definition). This is reported and left out rather than attempted.
- 8 of 11 sheet metal parts convert. The rest stay solid bodies (still placed and joined) and are flagged in the report and the parts table:
  - A pocket in a panel deeper than half the sheet: not supported by `sheet.convert` yet; converting and then cutting the pocket by hand works.
  - A body whose faces don't stitch into one closed sheet (`sew: the faces do not close`): a geometry issue in the source STEP.
  - Two cases where one bend is the problem and SolidCraft names it in the error with its point: a bend that would close a loop of bends (the sheet can't fold flat with every edge a bend), and a bend that runs along a face it meets at more than one edge (`a chamfer picked as a bend that runs along a face more than once`). The profile's `sheet_convert` op (`retry_dropping_point`) retries without the bend nearest the point the error names, leaving that edge cut open instead, which the runner and `Run-BuildSteps.ps1` both do automatically.
- The repository's own generated test fixture (`crates/lcw-analyze/tests/bracket_rack.stp`, built by `tools/make_fixtures.py`) has one bend SolidCraft's mesher doesn't recognise as lying between two faces, even though the two faces correctly share one `EDGE_CURVE` in the STEP (checked by hand). Real Inventor-exported bends at the same scale convert without issue, so this looks like a tessellation corner case triggered by the fixture's minimal, hand-built geometry rather than a bug in inv2sc's bend-point selection; it is not blocking, since the fixture is a test aid, not shipped data.
