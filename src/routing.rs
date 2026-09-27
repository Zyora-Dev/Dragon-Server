use crate::config::{Match, Route};

pub fn normalize_host(value: &str) -> Option<String> {
    if value == "*" {
        return Some(value.into());
    }
    if value.is_empty() || !value.is_ascii() || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    let authority: hyper::http::uri::Authority = value.parse().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let host = authority.host();
    let suffix = authority.as_str().strip_prefix(host)?;
    if !suffix.is_empty() {
        suffix.strip_prefix(':')?.parse::<u16>().ok()?;
    }
    if host.starts_with('[') {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::Ipv6Addr>()
            .ok()?;
    } else if !host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

pub fn decode_path(raw: &str) -> Result<String, &'static str> {
    if !raw.starts_with('/') {
        return Err("origin-form target required");
    }
    let mut decoded = Vec::with_capacity(raw.len());
    let mut source = raw.bytes();
    while let Some(byte) = source.next() {
        let value = if byte == b'%' {
            let high = source
                .next()
                .and_then(|value| (value as char).to_digit(16))
                .ok_or("bad escape")?;
            let low = source
                .next()
                .and_then(|value| (value as char).to_digit(16))
                .ok_or("bad escape")?;
            let value = (high * 16 + low) as u8;
            if value == b'/' || value == b'\\' || value == b'%' {
                return Err("ambiguous encoded separator");
            }
            value
        } else {
            byte
        };
        if value < 32 || value == 127 || value == b'\\' || value == b'#' {
            return Err("invalid path character");
        }
        decoded.push(value);
    }
    let path = String::from_utf8(decoded).map_err(|_| "path must be UTF-8")?;
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
        || path.contains("//")
    {
        return Err("ambiguous path segment");
    }
    Ok(path)
}

pub fn select_route<'route>(routes: &'route [Route], path: &str) -> Option<(usize, &'route Route)> {
    routes
        .iter()
        .enumerate()
        .filter(|(_, route)| match route.matcher {
            Match::Exact => route.path == path,
            Match::Prefix => {
                route.path == "/"
                    || route.path == path
                    || path
                        .strip_prefix(&route.path)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }
        })
        .max_by_key(|(_, route)| (route.matcher == Match::Exact, route.path.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_paths() {
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/%252e%252e/secret",
            "/a%2fb",
            "/%00",
            "/%zz",
            "/a\\b",
            "/a//b",
        ] {
            assert!(decode_path(path).is_err(), "{path}");
        }
        assert_eq!(decode_path("/hello%20world").unwrap(), "/hello world");
    }

    #[test]
    fn validates_authorities() {
        assert_eq!(normalize_host("LOCALHOST:8080").unwrap(), "localhost:8080");
        for host in [
            "user@localhost",
            "bad host",
            "localhost:99999",
            "localhost:",
            "-bad.test",
        ] {
            assert!(normalize_host(host).is_none(), "{host}");
        }
    }
}
