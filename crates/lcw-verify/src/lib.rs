//! `inv2sc verify`: an independent check that a SolidCraft import is correct.
//!
//! It asks SolidCraft what it holds (the built assembly's tree and placements, each part
//! design's volume and box, each instance's centre of mass, inertia and box in the world, the
//! interference check) and compares that with what the bundle says Inventor had. The expected
//! world placements are computed here from the bundle's matrices; the converter's own analysis
//! and build steps are not used.

pub mod assign;
pub mod expect;
pub mod judge;
pub mod obs;
pub mod report;
pub mod session;
pub mod verify;

pub use judge::Tol;
pub use verify::{run, Check, Inputs, Report, Status};
