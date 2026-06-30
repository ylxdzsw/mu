use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{HttpRequest, MAX_REQUEST_BYTES};

pub(super) fn parse_socket_mode(value: &str) -> Result<u32> {
    u32::from_str_radix(value.trim_start_matches('0'), 8)
        .with_context(|| format!("invalid socket mode `{value}`"))
}

pub(super) fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)
            .with_context(|| format!("removing stale {}", path.display()))?,
        Ok(_) => bail!("refusing to replace non-socket path: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    }
    Ok(())
}

pub(super) async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            bail!("connection closed before request headers");
        }
        buffer.extend_from_slice(&temp[..n]);
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
    };

    let (method, uri, content_length) = {
        let header =
            std::str::from_utf8(&buffer[..header_end]).context("request header is not UTF-8")?;
        let mut lines = header.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing request line"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let uri = parts.next().unwrap_or_default().to_string();
        let mut content_length = 0usize;
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().context("invalid content-length")?;
            }
        }
        (method, uri, content_length)
    };

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            bail!("connection closed before request body");
        }
        buffer.extend_from_slice(&temp[..n]);
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
    }
    let (path, query) = parse_uri(&uri);
    Ok(HttpRequest {
        method,
        path,
        query,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

pub(super) fn parse_uri(uri: &str) -> (String, HashMap<String, String>) {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(key), percent_decode(value));
    }
    (percent_decode(path), map)
}

pub(super) fn query_flag(query: &HashMap<String, String>, key: &str) -> bool {
    query.get(key).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub(super) async fn write_json_response<T: Serialize + ?Sized>(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write_response(
        stream,
        status,
        reason,
        &[("content-type", "application/json; charset=utf-8")],
        &body,
    )
    .await
}

pub(super) async fn write_response(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("content-security-policy: default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'\r\n");
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

pub(super) async fn write_sse_headers(stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\nconnection: close\r\ncache-control: no-cache, no-transform\r\nx-accel-buffering: no\r\nx-content-type-options: nosniff\r\n\r\n",
        )
        .await?;
    stream.flush().await?;
    Ok(())
}

pub(super) async fn write_sse_event<T: Serialize + ?Sized>(
    stream: &mut (impl AsyncWrite + Unpin),
    id: Option<u64>,
    event: &str,
    payload: &T,
) -> Result<()> {
    let data = serde_json::to_string(payload)?;
    let mut frame = String::new();
    if let Some(id) = id {
        frame.push_str("id: ");
        frame.push_str(&id.to_string());
        frame.push('\n');
    }
    frame.push_str("event: ");
    frame.push_str(event);
    frame.push('\n');
    frame.push_str("data: ");
    frame.push_str(&data);
    frame.push_str("\n\n");
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((a << 4) | b);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
