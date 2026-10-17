//! The heart of the Form Designer.
//!
//! * [`catalog`] describes every control the Tool Palette offers: its Slint
//!   element, editable properties, callbacks and the property that is
//!   published to Rust.
//! * [`model`] is the in-memory form the designer edits.
//! * [`writer`] turns a form into the canonical `.slint` text, and [`reader`]
//!   parses that canonical subset back.
//! * [`codegen`] produces the Rust wiring module and handler stubs.

pub mod catalog;
pub mod codegen;
pub mod model;
pub mod reader;
pub mod writer;

pub use catalog::{kind_spec, ArgSpec, CallbackSpec, KindSpec, PropSpec, PropType, KINDS};
pub use model::{snap, to_snake_case, Control, Form, FormError, HandlerRef, GRID};
pub use reader::{read_slint, ReadError};
pub use writer::write_slint;
