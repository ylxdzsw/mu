use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::provider::{Attachment, ContentPart};

pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

pub fn load_attachments(paths: &[PathBuf]) -> Result<Vec<ContentPart>> {
    paths
        .iter()
        .map(|path| load_attachment(path).map(|attachment| ContentPart::Attachment { attachment }))
        .collect()
}

pub fn load_attachment(path: &Path) -> Result<Attachment> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading attachment metadata {}", path.display()))?;
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        bail!(
            "attachment exceeds 20 MiB limit: {} ({} bytes)",
            path.display(),
            metadata.len()
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("reading attachment {}", path.display()))?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        bail!(
            "attachment exceeds 20 MiB limit: {} ({} bytes)",
            path.display(),
            bytes.len()
        );
    }
    let media_type = attachment_media_type(path, &bytes)?;
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    Ok(Attachment {
        filename,
        media_type: media_type.to_string(),
        data: bytes,
    })
}

pub fn attachment_media_type(path: &Path, bytes: &[u8]) -> Result<&'static str> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let detected = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("jpeg", "image/jpeg"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some(("wav", "audio/wav"))
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        Some(("mp3", "audio/mpeg"))
    } else {
        None
    };
    let Some((format, media_type)) = detected else {
        bail!("unsupported attachment type: {}", path.display());
    };
    let extension_matches = match format {
        "jpeg" => matches!(extension.as_str(), "jpg" | "jpeg"),
        other => extension == other,
    };
    if !extension_matches {
        bail!(
            "attachment extension does not match detected {format} content: {}",
            path.display()
        );
    }
    Ok(media_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_media_types_and_mismatches() {
        assert_eq!(
            attachment_media_type(Path::new("image.png"), b"\x89PNG\r\n\x1a\nrest").unwrap(),
            "image/png"
        );
        assert_eq!(
            attachment_media_type(Path::new("sound.mp3"), b"ID3rest").unwrap(),
            "audio/mpeg"
        );
        assert!(attachment_media_type(Path::new("wrong.jpg"), b"\x89PNG\r\n\x1a\nrest").is_err());
    }
}
