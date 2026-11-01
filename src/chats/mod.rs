//! WhatsApp and Matrix. Both speak the same small protocol to the app: the
//! WhatsApp helper (the Go program in `wa/`) over its stdin and stdout, and
//! Matrix from a task here (`matrix`), so the app treats them alike.

#[cfg(feature = "matrix")]
pub mod matrix;
pub mod wa;
