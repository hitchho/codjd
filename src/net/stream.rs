//! Serving a file with HTTP Range support (seeking in apps depends on it)

use std::path::Path;

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

#[derive(Debug, PartialEq, Eq)]
pub enum RangeSpec {
    /// no usable Range header: the whole file
    Full,
    /// first and last byte, inclusive
    Partial(u64, u64),
    /// starts past the end
    Unsatisfiable,
}

/// Parses a `Range` header against a file of `len` bytes. Only a single range is served; a
/// multi-range or malformed header is ignored (a whole-file answer is always valid).
pub fn parse_range(header: Option<&str>, len: u64) -> RangeSpec {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if spec.contains(',') || len == 0 {
        return RangeSpec::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(start), Ok(end)) if end >= start => {
            if start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, end.min(len - 1))
            }
        }
        (Ok(start), Err(_)) if b.is_empty() => {
            if start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(start, len - 1)
            }
        }
        // suffix: the last n bytes
        (Err(_), Ok(n)) if a.is_empty() => {
            if n == 0 {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Partial(len.saturating_sub(n), len - 1)
            }
        }
        _ => RangeSpec::Full,
    }
}

/// Answers with the file (or the requested part of it). `None` if it can't be opened.
pub async fn serve_file(
    path: &Path,
    content_type: &str,
    request: &HeaderMap,
    download_name: Option<&str>,
) -> Option<Response> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    let range = parse_range(request.get(header::RANGE).and_then(|v| v.to_str().ok()), len);
    let mut res = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(name) = download_name {
        // quotes and control characters out of the header
        let safe: String = name.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect();
        res = res.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{safe}\""));
    }
    let (status, from, count) = match range {
        RangeSpec::Full => (StatusCode::OK, 0, len),
        RangeSpec::Partial(a, b) => {
            res = res.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"));
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        RangeSpec::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{len}")).ok()?)
                .body(Body::empty())
                .ok();
        }
    };
    if from > 0 {
        file.seek(std::io::SeekFrom::Start(from)).await.ok()?;
    }
    res.status(status)
        .header(header::CONTENT_LENGTH, count)
        .body(Body::from_stream(ReaderStream::with_capacity(file.take(count), 64 * 1024)))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        use RangeSpec::*;
        assert_eq!(parse_range(None, 100), Full);
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Partial(0, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 100), Partial(10, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Partial(90, 99));
        assert_eq!(parse_range(Some("bytes=-500"), 100), Partial(0, 99));
        assert_eq!(parse_range(Some("bytes=90-500"), 100), Partial(90, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=100-200"), 100), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 100), Unsatisfiable);
        // ignored: reversed, multi-range, garbage, other units, empty file
        assert_eq!(parse_range(Some("bytes=9-0"), 100), Full);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Full);
        assert_eq!(parse_range(Some("bytes=abc"), 100), Full);
        assert_eq!(parse_range(Some("items=0-1"), 100), Full);
        assert_eq!(parse_range(Some("bytes=0-1"), 0), Full);
        assert_eq!(parse_range(Some("bytes=-"), 100), Full);
    }
}
