//! Projects and cargo.
//!
//! * [`project`] creates and opens Ferrite projects (an ordinary cargo
//!   package plus `Ferrite.toml`) and produces the files a form owns.
//! * [`cargo`] runs `cargo build` with JSON messages and turns them into
//!   problems and the executable path, and runs the built program.

pub mod cargo;
pub mod project;

pub use cargo::{BuildOptions, CargoEvent, Job, Problem};
pub use ferrite_rpc::msys2_roots;
pub use project::{FormEntry, FormOutputs, Manifest, Project};
