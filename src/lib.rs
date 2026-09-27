pub mod config;
mod ingress;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod process;
pub mod routing;
pub mod server;
mod static_files;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_available() {
        assert_eq!(super::version(), "0.1.0");
    }
}
