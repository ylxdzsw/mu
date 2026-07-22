use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::attachment::{MAX_ATTACHMENT_BYTES, attachment_media_type};
use crate::provider::{Attachment, ImageDetail, ToolArtifact};

pub const ARTIFACT_DIR_ENV: &str = "MU_ARTIFACT_DIR";
pub const MAX_TOOL_ARTIFACTS: usize = 8;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const COMMITTED_EXTENSION: &str = "artifact";

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
    let dir = std::env::var_os(ARTIFACT_DIR_ENV)
        .with_context(|| "view_image must run inside a Mu bash tool call")?;
    let dir = crate::windows_msys2::native_env_path(&dir);
    let order = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stem = format!(
        "{order:039}-{:010}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let temporary = dir.join(format!("{stem}.tmp"));
    let committed = dir.join(format!("{stem}.{COMMITTED_EXTENSION}"));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating artifact spool file {}", temporary.display()))?;
        write_image_record(&mut file, attachment, detail)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &committed)
            .with_context(|| format!("committing artifact {}", committed.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
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

pub fn read_artifacts(dir: &Path) -> Result<Vec<ToolArtifact>> {
    let mut files = std::fs::read_dir(dir)
        .with_context(|| format!("reading artifact spool {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == COMMITTED_EXTENSION)
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut artifacts = Vec::new();
    for path in files {
        if artifacts.len() >= MAX_TOOL_ARTIFACTS {
            bail!("bash emitted more than {MAX_TOOL_ARTIFACTS} artifacts");
        }
        let mut reader = File::open(&path)
            .with_context(|| format!("opening artifact record {}", path.display()))?;
        let mut header_len = [0u8; 4];
        loop {
            match reader.read(&mut header_len[..1]) {
                Ok(0) => bail!("empty artifact record {}", path.display()),
                Ok(_) => {
                    reader.read_exact(&mut header_len[1..])?;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
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
        let mut trailing = [0u8; 1];
        if reader.read(&mut trailing)? != 0 {
            bail!("trailing bytes in artifact record {}", path.display());
        }
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
        let dir = std::env::temp_dir().join(format!("mu-artifacts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("0001.artifact");
        let mut file = File::create(path).unwrap();
        write_image_record(&mut file, &attachment, ImageDetail::High).unwrap();
        drop(file);
        let artifacts = read_artifacts(&dir).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].attachment, attachment);
        assert_eq!(artifacts[0].detail, ImageDetail::High);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
