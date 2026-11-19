//! The SolidCraft side of the bridge: which commands to use (the profile, checked against the
//! running SolidCraft), the build steps for parts and assemblies, and running them over
//! SolidCraft's MCP server.

pub mod emit;
pub mod mcp;
pub mod profile;
pub mod progress;
pub mod run;
pub mod steps;
