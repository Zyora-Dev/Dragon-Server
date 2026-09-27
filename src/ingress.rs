use crate::config::Limits;
use bytes::{Buf, BytesMut};
use std::io;

enum Stage {
    Head,
    Fixed(u64),
    ChunkSize,
    ChunkData(u64),
    ChunkEnd,
    Trailers,
}

pub(crate) struct Ingress {
    bytes: BytesMut,
    approved: usize,
    stage: Stage,
    limits: Limits,
}

impl Ingress {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            bytes: BytesMut::new(),
            approved: 0,
            stage: Stage::Head,
            limits,
        }
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    pub(crate) fn capacity(&self) -> usize {
        self.limits
            .max_header_bytes
            .saturating_sub(self.bytes.len())
            .min(16_384)
    }

    pub(crate) fn deliver(&mut self, destination: &mut tokio::io::ReadBuf<'_>) -> io::Result<bool> {
        if self.approved == 0 {
            self.approved = self.inspect()?.unwrap_or(0);
        }
        if self.approved == 0 {
            return Ok(false);
        }
        let count = self.approved.min(destination.remaining());
        destination.put_slice(&self.bytes[..count]);
        self.bytes.advance(count);
        self.approved -= count;
        Ok(true)
    }

    fn inspect(&mut self) -> io::Result<Option<usize>> {
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "rejected HTTP framing or head limit",
            )
        };
        match self.stage {
            Stage::Head => {
                let mut headers = vec![httparse::EMPTY_HEADER; self.limits.max_headers];
                let mut request = httparse::Request::new(&mut headers);
                let parsed = request.parse(&self.bytes).map_err(|_| bad())?;
                let httparse::Status::Complete(length) = parsed else {
                    if self.capacity() == 0 {
                        return Err(bad());
                    }
                    return Ok(None);
                };
                if length > self.limits.max_header_bytes {
                    return Err(bad());
                }
                let mut content_length = None;
                let mut chunked = false;
                for header in request.headers {
                    if header.name.eq_ignore_ascii_case("content-length") {
                        let value = std::str::from_utf8(header.value).map_err(|_| bad())?;
                        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                            return Err(bad());
                        }
                        let value: u64 = value.parse().map_err(|_| bad())?;
                        if content_length.is_some_and(|previous| previous != value) {
                            return Err(bad());
                        }
                        content_length = Some(value);
                    }
                    if header.name.eq_ignore_ascii_case("transfer-encoding") {
                        if chunked || !header.value.eq_ignore_ascii_case(b"chunked") {
                            return Err(bad());
                        }
                        chunked = true;
                    }
                }
                if chunked && content_length.is_some() {
                    return Err(bad());
                }
                self.stage = if chunked {
                    Stage::ChunkSize
                } else if let Some(size @ 1..) = content_length {
                    Stage::Fixed(size)
                } else {
                    Stage::Head
                };
                Ok(Some(length))
            }
            Stage::Fixed(remaining) | Stage::ChunkData(remaining) => {
                let count = remaining.min(self.bytes.len() as u64) as usize;
                if count == 0 {
                    return Ok(None);
                }
                let remaining = remaining - count as u64;
                self.stage = match (&self.stage, remaining) {
                    (Stage::Fixed(_), 0) => Stage::Head,
                    (Stage::Fixed(_), _) => Stage::Fixed(remaining),
                    (_, 0) => Stage::ChunkEnd,
                    _ => Stage::ChunkData(remaining),
                };
                Ok(Some(count))
            }
            Stage::ChunkSize => {
                match httparse::parse_chunk_size(&self.bytes).map_err(|_| bad())? {
                    httparse::Status::Complete((length, size)) => {
                        if length > 1024 {
                            return Err(bad());
                        }
                        self.stage = if size == 0 {
                            Stage::Trailers
                        } else {
                            Stage::ChunkData(size)
                        };
                        Ok(Some(length))
                    }
                    httparse::Status::Partial => {
                        if self.bytes.len() >= 1024 {
                            return Err(bad());
                        }
                        Ok(None)
                    }
                }
            }
            Stage::ChunkEnd => {
                if self.bytes.len() < 2 {
                    return Ok(None);
                }
                if &self.bytes[..2] != b"\r\n" {
                    return Err(bad());
                }
                self.stage = Stage::ChunkSize;
                Ok(Some(2))
            }
            Stage::Trailers => {
                let mut headers = vec![httparse::EMPTY_HEADER; self.limits.max_headers];
                match httparse::parse_headers(&self.bytes, &mut headers).map_err(|_| bad())? {
                    httparse::Status::Complete((length, headers)) => {
                        if length > self.limits.max_header_bytes
                            || headers.iter().any(|header| {
                                ["content-length", "transfer-encoding", "host"]
                                    .iter()
                                    .any(|name| header.name.eq_ignore_ascii_case(name))
                            })
                        {
                            return Err(bad());
                        }
                        self.stage = Stage::Head;
                        Ok(Some(length))
                    }
                    httparse::Status::Partial => {
                        if self.capacity() == 0 {
                            return Err(bad());
                        }
                        Ok(None)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::ReadBuf;

    fn pass_fragments(input: &[u8], split: usize, output_size: usize) -> io::Result<Vec<u8>> {
        let mut ingress = Ingress::new(Limits::default());
        let mut output = Vec::new();
        for fragment in [&input[..split], &input[split..]] {
            for piece in fragment.chunks(37) {
                assert!(piece.len() <= ingress.capacity());
                ingress.append(piece);
                loop {
                    let mut buffer = vec![0; output_size];
                    let mut destination = ReadBuf::new(&mut buffer);
                    if !ingress.deliver(&mut destination)? {
                        break;
                    }
                    assert!(!destination.filled().is_empty());
                    output.extend_from_slice(destination.filled());
                }
            }
        }
        Ok(output)
    }

    #[test]
    fn preserves_framing_at_every_fragment_boundary() {
        let requests: &[&[u8]] = &[
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nabc\r\n\r\ndefGET /next HTTP/1.1\r\nHost: localhost\r\n\r\n",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3;name=value\r\nabc\r\n2\r\nde\r\n0\r\nX-Check: yes\r\n\r\nGET /next HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ];
        for request in requests {
            for split in 0..=request.len() {
                for output_size in [1, 7, 128] {
                    assert_eq!(
                        pass_fragments(request, split, output_size).unwrap(),
                        *request
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_ambiguous_heads_at_every_fragment_boundary() {
        let headers = [
            "Transfer-Encoding: chunked\r\nContent-Length: 5\r\n",
            "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: +5\r\n",
            "Content-Length: 5, 5\r\n",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
            "Transfer-Encoding: gzip, chunked\r\n",
        ];
        for headers in headers {
            let request = format!("POST / HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n");
            for split in 0..=request.len() {
                assert!(
                    pass_fragments(request.as_bytes(), split, 7).is_err(),
                    "{headers}, split {split}"
                );
            }
            let mut ingress = Ingress::new(Limits::default());
            ingress.append(request.as_bytes());
            let mut buffer = [0; 256];
            let mut destination = ReadBuf::new(&mut buffer);
            assert!(ingress.deliver(&mut destination).is_err());
            assert!(destination.filled().is_empty());
        }
    }

    #[test]
    fn rejects_forbidden_trailers_and_oversized_chunk_lines() {
        for trailer in [
            "Host: other",
            "Content-Length: 0",
            "Transfer-Encoding: chunked",
        ] {
            let request = format!(
                "POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n{trailer}\r\n\r\n"
            );
            for split in 0..=request.len() {
                assert!(pass_fragments(request.as_bytes(), split, 3).is_err());
            }
        }
        let request = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n1;{}\r\nx\r\n0\r\n\r\n",
            "a".repeat(1024)
        );
        assert!(pass_fragments(request.as_bytes(), 0, 7).is_err());
    }
}
