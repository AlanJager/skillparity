//! SkillParity runs one skill's `parity.yaml` tasks on a harness and asserts
//! trigger, tools, files, and stdout.

pub mod adapters;
pub mod assertions;
pub mod ci;
pub mod cli;
pub mod model;
pub mod process;
pub mod report;
pub mod run;
pub mod skill_fs;
pub mod timefmt;
pub mod which_path;
pub mod yaml;
