#![no_main]

pub use dragon_server::config;
#[path = "../../src/ingress.rs"]
mod ingress;

use dragon_server::routing::{decode_path, normalize_host};
use libfuzzer_sys::fuzz_target;
use tokio::io::ReadBuf;

fuzz_target!(|data: &[u8]| {
    if data.len() > 65_536 {
        return;
    }
    let mut previous = None;
    for fragment_size in [1, 37, 16_384] {
        let mut guard = ingress::Ingress::new(config::Limits::default());
        let mut position = 0;
        let mut delivered = 0;
        let mut failed = false;
        while position < data.len() && !failed && !guard.awaiting_upgrade() {
            let count = fragment_size
                .min(guard.capacity())
                .min(data.len() - position);
            assert!(count > 0, "ingress stalled with a full buffer");
            guard.append(&data[position..position + count]);
            position += count;
            loop {
                let mut buffer = [0; 257];
                let mut destination = ReadBuf::new(&mut buffer);
                match guard.deliver(&mut destination) {
                    Ok(true) => {
                        let output = destination.filled();
                        assert!(!output.is_empty());
                        assert!(delivered + output.len() <= position);
                        assert_eq!(output, &data[delivered..delivered + output.len()]);
                        delivered += output.len();
                    }
                    Ok(false) => break,
                    Err(_) => {
                        assert!(destination.filled().is_empty());
                        failed = true;
                        break;
                    }
                }
            }
        }
        if let Some((previous_failed, previous_delivered)) = previous {
            assert_eq!(failed, previous_failed, "fragment-dependent acceptance");
            if !failed {
                assert_eq!(delivered, previous_delivered);
            }
        }
        previous = Some((failed, delivered));
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(path) = decode_path(text) {
            assert!(path.starts_with('/'));
            assert!(!path.contains(['\\', '%', '#']));
            assert!(!path.contains("//"));
            assert!(!path.split('/').any(|part| part == "." || part == ".."));
            assert_eq!(decode_path(&path).as_deref(), Ok(path.as_str()));
        }
        if let Some(host) = normalize_host(text) {
            assert_eq!(normalize_host(&host), Some(host));
        }
        if let Ok(mut parsed) = toml::from_str::<config::Config>(text) {
            let _ = parsed.validate(std::path::Path::new("."));
        }
    }
});
