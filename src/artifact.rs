use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::attachment::{MAX_ATTACHMENT_BYTES, attachment_media_type};
use crate::provider::{Attachment, ImageDetail, ToolArtifact};

pub const ARTIFACT_FD_ENV: &str = "MU_ARTIFACT_FD";
pub const ARTIFACT_FD: RawFd = 3;
pub const MAX_TOOL_ARTIFACTS: usize = 8;
const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct ImageArtifactHeader {
    version: u8,
    kind: String,
    filename: String,
    media_type: String,
    detail: ImageDetail,
    byte_length: u64,
}

pub fn write_image_artifact(attachment: &Attachment, detail: ImageDetail) -> Result<()> {
    let fd = std::env::var(ARTIFACT_FD_ENV)
        .with_context(|| "view_image must run inside a Mu bash tool call")?;
    let fd = RawFd::from_str(&fd).context("invalid Mu artifact file descriptor")?;
    if fd < 0 {
        bail!("invalid Mu artifact file descriptor");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    write_image_record(&mut file, attachment, detail)
}

fn write_image_record(
    writer: &mut impl Write,
    attachment: &Attachment,
    detail: ImageDetail,
) -> Result<()> {
    let header = ImageArtifactHeader {
        version: 1,
        kind: "image".into(),
        filename: attachment.filename.clone(),
        media_type: attachment.media_type.clone(),
        detail,
        byte_length: attachment.data.len() as u64,
    };
    let header = serde_json::to_vec(&header)?;
    let header_len = u32::try_from(header.len()).context("artifact header is too large")?;
    writer.write_all(&header_len.to_be_bytes())?;
    writer.write_all(&header)?;
    writer.write_all(&(attachment.data.len() as u64).to_be_bytes())?;
    writer.write_all(&attachment.data)?;
    writer.flush()?;
    Ok(())
}

pub fn read_artifacts(mut reader: impl Read) -> Result<Vec<ToolArtifact>> {
    let mut artifacts = Vec::new();
    loop {
        let mut header_len = [0u8; 4];
        match reader.read(&mut header_len[..1]) {
            Ok(0) => break,
            Ok(_) => reader.read_exact(&mut header_len[1..])?,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
        if artifacts.len() >= MAX_TOOL_ARTIFACTS {
            bail!("bash emitted more than {MAX_TOOL_ARTIFACTS} artifacts");
        }
        let header_len = u32::from_be_bytes(header_len) as usize;
        if header_len == 0 || header_len > MAX_HEADER_BYTES {
            bail!("invalid artifact header length {header_len}");
        }
        let mut header = vec![0; header_len];
        reader.read_exact(&mut header)?;
        let header: ImageArtifactHeader =
            serde_json::from_slice(&header).context("decoding artifact header")?;
        if header.version != 1 || header.kind != "image" {
            bail!("unsupported artifact record");
        }
        if !header.media_type.starts_with("image/") {
            bail!("invalid image artifact media type {}", header.media_type);
        }
        let mut payload_len = [0u8; 8];
        reader.read_exact(&mut payload_len)?;
        let payload_len = u64::from_be_bytes(payload_len);
        if payload_len != header.byte_length {
            bail!("artifact payload length does not match its header");
        }
        if payload_len > MAX_ATTACHMENT_BYTES {
            bail!("artifact exceeds 20 MiB limit");
        }
        let mut data = vec![0; payload_len as usize];
        reader.read_exact(&mut data)?;
        let detected = attachment_media_type(Path::new(&header.filename), &data)?;
        if detected != header.media_type {
            bail!(
                "artifact media type {} does not match detected {detected}",
                header.media_type
            );
        }
        artifacts.push(ToolArtifact {
            attachment: Attachment {
                filename: header.filename,
                media_type: header.media_type,
                data,
            },
            detail: header.detail,
        });
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_record_round_trips() {
        let attachment = Attachment {
            filename: "tiny.png".into(),
            media_type: "image/png".into(),
            data: b"\x89PNG\r\n\x1a\nrest".to_vec(),
        };
        let mut bytes = Vec::new();
        write_image_record(&mut bytes, &attachment, ImageDetail::High).unwrap();
        let artifacts = read_artifacts(bytes.as_slice()).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].attachment, attachment);
        assert_eq!(artifacts[0].detail, ImageDetail::High);
    }
}
