# Laser Cut Workbench and SolidCraft

Laser Cut Workbench (LCW) is today a Python server with an HTML interface, driving Inventor through PowerShell. This repository is laid out so LCW can move onto the same Rust core one piece at a time, with no big rewrite.

## What LCW gets from these crates now

| LCW today | Shared crate |
|---|---|
| `Import Inventor.ps1` walks the assembly, finds sheet metal parts, exports flat DXFs and bend results | `inventor/Export-InventorBundle.ps1` writes the same data (and more) as a versioned bundle; LCW can read `bundle.json` instead of its own JSON |
| Plate detection (`GetVerifiedPlanarPlate`: opposite faces, every vertex between them) | `lcw-analyze::sheet` (the same test, plus bends) |
| Quantities by occurrence count | `lcw-analyze::model::Model::instances` |
| Bend direction, angle and radius from flat bend results | `lcw-bundle::FlatBend` |

## Suggested path

1. **Read bundles in LCW.** `server.py` calls `Export-InventorBundle.ps1` instead of its own importer, and reads `bundle.json` plus `flat/*.dxf`. Nothing else changes.
2. **A Rust LCW core.** Port `geometry.py`, nesting and cut order into new crates (`lcw-nest`, `lcw-dxf`). Expose them to the Python server through a small CLI that speaks JSON, the way `inv2sc` does, until the server itself moves.
3. **A Rust LCW app.** Replace the Python server with a Rust one (serving the same HTML), or a native window. Ship it as one `.exe`.
4. **SolidCraft both ways.** LCW reads sheet metal flat patterns straight from SolidCraft designs (`sheet.flat_pattern`, `sheet.export_dxf`, `sheet.export_bend_table` over the same MCP runner). Designs built in SolidCraft can then go to the laser without Inventor.

At every step, the bundle format (`lcw-bundle/1`) and the command profile keep Inventor, LCW and SolidCraft decoupled: each can change without the others breaking.
