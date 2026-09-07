//! The output boundary: everything that decides *how* (and whether)
//! something is shown to the user lives under this module, kept separate
//! from `commands` (which only returns data) and `app` (which only
//! dispatches). `progress` is the transient, stderr-only sibling of
//! `render`'s durable `Outcome` rendering.

pub mod error;
pub mod notices;
pub mod progress;
pub mod render;
mod rows;
mod system;
mod terminal;
mod writer;
pub use writer::{Output, Stream, WriteFailure};

pub(crate) use terminal::help_styles;

/// Exercise the process adapter's ANSI stripping in renderer tests.
#[cfg(test)]
fn strip_ansi(text: &str) -> String {
    use std::io::Write;
    let mut output = anstream::StripStream::new(Vec::new());
    output.write_all(text.as_bytes()).unwrap();
    String::from_utf8(output.into_inner()).unwrap()
}
