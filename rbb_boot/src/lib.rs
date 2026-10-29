//! Early-boot primitives. Destructive operations are restricted to PID 1.
pub mod cmdline;
pub mod device;
pub mod fetch;
pub mod loader;
pub mod local;
pub mod model;
pub mod modules;
pub mod netboot;
pub mod network;
pub mod root;
pub mod verification;

pub use root::RootConfig;

pub fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}
