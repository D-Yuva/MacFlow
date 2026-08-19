#![deny(unsafe_op_in_unsafe_fn)]

pub mod core;

#[cfg(target_os = "macos")]
pub mod platform;

#[cfg(not(target_os = "macos"))]
compile_error!("MacFlow is a native macOS application");
