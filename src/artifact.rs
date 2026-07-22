use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::attachment::{MAX_ATTACHMENT_BYTES, attachment_media_type};
use crate::provider::{Attachment, ImageDetail, ToolArtifact};

pub const ARTIFACT_DIR_ENV: &str = "MU_ARTIFACT_DIR";
pub const MAX_TOOL_ARTIFACTS: usize = 8;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const ARTIFACT_SUFFIX: &str = ".artifact";

pub struct ArtifactDirectory {
    path: PathBuf,
}

impl ArtifactDirectory {
    pub fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("mu-artifacts-{}", Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&path)
            .with_context(|| format!("creating Mu artifact directory {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Vec<ToolArtifact>> {
        read_artifacts(&self.path)
    }
}

impl Drop for ArtifactDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

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
        .map(PathBuf::from)
        .with_context(|| "view_image must run inside a Mu bash tool call")?;
    write_image_artifact_to(&dir, attachment, detail)
}

fn write_image_artifact_to(dir: &Path, attachment: &Attachment, detail: ImageDetail) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)
        .with_context(|| "Mu artifact directory is no longer available")?;
    if !metadata.file_type().is_dir() {
        bail!("invalid Mu artifact directory");
    }

    let lock_path = dir.join(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .context("opening Mu artifact lock")?;
    lock.lock_exclusive()
        .context("locking Mu artifact directory")?;

    let sequence = (0..MAX_TOOL_ARTIFACTS)
        .find(|sequence| !artifact_path(dir, *sequence).exists())
        .with_context(|| format!("bash emitted more than {MAX_TOOL_ARTIFACTS} artifacts"))?;
    let final_path = artifact_path(dir, sequence);
    let temporary_path = dir.join(format!(".{sequence:08}-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
            .context("creating Mu artifact file")?;
        write_image_record(&mut file, attachment, detail)?;
        drop(file);
        fs::rename(&temporary_path, &final_path).context("publishing Mu artifact")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    let unlock_result = FileExt::unlock(&lock).context("unlocking Mu artifact directory");
    result.and(unlock_result)
}

fn artifact_path(dir: &Path, sequence: usize) -> PathBuf {
    dir.join(format!("{sequence:08}{ARTIFACT_SUFFIX}"))
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
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading Mu artifact directory {}", dir.display()))?
    {
        let path = entry.context("reading Mu artifact directory entry")?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(ARTIFACT_SUFFIX))
        {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.len() > MAX_TOOL_ARTIFACTS {
        bail!("bash emitted more than {MAX_TOOL_ARTIFACTS} artifacts");
    }

    paths
        .iter()
        .map(|path| {
            let mut file = File::open(path)
                .with_context(|| format!("opening Mu artifact {}", path.display()))?;
            let artifact = read_image_record(&mut file)?;
            let mut trailing = [0u8; 1];
            if file.read(&mut trailing)? != 0 {
                bail!("artifact record has trailing data");
            }
            Ok(artifact)
        })
        .collect()
}

fn read_image_record(reader: &mut impl Read) -> Result<ToolArtifact> {
    let mut header_len = [0u8; 4];
    reader.read_exact(&mut header_len)?;
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
    Ok(ToolArtifact {
        attachment: Attachment {
            filename: header.filename,
            media_type: header.media_type,
            data,
        },
        detail: header.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(filename: &str) -> Attachment {
        Attachment {
            filename: filename.into(),
            media_type: "image/png".into(),
            data: b"\x89PNG\r\n\x1a\nrest".to_vec(),
        }
    }

    #[test]
    fn artifact_directory_round_trips_images_in_order() {
        let dir = ArtifactDirectory::create().unwrap();
        write_image_artifact_to(dir.path(), &attachment("first.png"), ImageDetail::High).unwrap();
        write_image_artifact_to(dir.path(), &attachment("second.png"), ImageDetail::Low).unwrap();

        let artifacts = dir.read().unwrap();
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].attachment.filename, "first.png");
        assert_eq!(artifacts[0].detail, ImageDetail::High);
        assert_eq!(artifacts[1].attachment.filename, "second.png");
        assert_eq!(artifacts[1].detail, ImageDetail::Low);
    }

    #[test]
    fn removed_artifact_directory_is_not_recreated() {
        let dir = ArtifactDirectory::create().unwrap();
        let path = dir.path().to_path_buf();
        fs::remove_dir_all(&path).unwrap();

        let error =
            write_image_artifact_to(&path, &attachment("late.png"), ImageDetail::Auto).unwrap_err();
        assert!(error.to_string().contains("no longer available"));
        assert!(!path.exists());
    }

    #[test]
    fn artifact_directory_is_removed_on_drop() {
        let path = {
            let dir = ArtifactDirectory::create().unwrap();
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn incomplete_temporary_files_are_not_artifacts() {
        let dir = ArtifactDirectory::create().unwrap();
        File::create(dir.path().join(".00000000-stale.tmp")).unwrap();
        assert!(dir.read().unwrap().is_empty());
    }

    #[test]
    fn concurrent_writers_publish_distinct_records() {
        let dir = ArtifactDirectory::create().unwrap();
        let writers = (0..4)
            .map(|index| {
                let path = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    write_image_artifact_to(
                        &path,
                        &attachment(&format!("{index}.png")),
                        ImageDetail::Auto,
                    )
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }

        let mut filenames = dir
            .read()
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.attachment.filename)
            .collect::<Vec<_>>();
        filenames.sort();
        assert_eq!(filenames, ["0.png", "1.png", "2.png", "3.png"]);
    }

    #[test]
    fn rejects_more_than_eight_artifacts() {
        let dir = ArtifactDirectory::create().unwrap();
        for sequence in 0..MAX_TOOL_ARTIFACTS {
            write_image_artifact_to(
                dir.path(),
                &attachment(&format!("{sequence}.png")),
                ImageDetail::Auto,
            )
            .unwrap();
        }

        let error =
            write_image_artifact_to(dir.path(), &attachment("extra.png"), ImageDetail::Auto)
                .unwrap_err();
        assert!(error.to_string().contains("more than 8 artifacts"));
    }

    #[test]
    fn rejects_trailing_record_data() {
        let dir = ArtifactDirectory::create().unwrap();
        let path = artifact_path(dir.path(), 0);
        let mut file = File::create(&path).unwrap();
        write_image_record(&mut file, &attachment("tiny.png"), ImageDetail::High).unwrap();
        file.write_all(b"trailing").unwrap();
        drop(file);

        let error = dir.read().unwrap_err();
        assert!(error.to_string().contains("trailing data"));
    }
}
