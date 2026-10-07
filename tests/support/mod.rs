pub fn resource_snapshot() -> (usize, u64) {
    let descriptor_path = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let descriptors = std::fs::read_dir(descriptor_path)
        .unwrap()
        .map(Result::unwrap)
        .count();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let rss_kib = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (descriptors, rss_kib)
}
