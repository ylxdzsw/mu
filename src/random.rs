use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Read bytes from the operating system's cryptographic random source.
///
/// Mu only targets Unix-like systems today, and `/dev/urandom` is available
/// on the supported platforms, including macOS. Keeping this small wrapper
/// here avoids using an application-level UUID as a pathname primitive.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    let mut source = File::open("/dev/urandom").context("opening OS random source")?;
    source
        .read_exact(&mut bytes)
        .context("reading OS random bytes")?;
    Ok(bytes)
}

pub fn session_id() -> Result<String> {
    let bytes = random_bytes::<5>()?;
    let mut value = 0u64;
    for byte in bytes {
        value = (value << 8) | u64::from(byte);
    }
    let mut suffix = [b'0'; 8];
    for index in (0..8).rev() {
        suffix[index] = CROCKFORD[(value & 0x1f) as usize];
        value >>= 5;
    }
    // The alphabet is deliberately lowercase so the identifier is stable on
    // case-folding filesystems even if a future caller uses it in a pathname.
    Ok(format!("ses_{}", String::from_utf8_lossy(&suffix)))
}

/// Create a private, unpredictably named file in `directory`.
///
/// `create_new` is the important part: a random name is only a hint until the
/// kernel performs the no-replace create atomically.
pub fn create_temp_file(directory: &Path, prefix: &str, suffix: &str) -> Result<(File, PathBuf)> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating temporary directory {}", directory.display()))?;
    for _ in 0..32 {
        let bytes = random_bytes::<12>()?;
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = directory.join(format!("{prefix}{token}{suffix}"));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary file {}", path.display()));
            }
        }
    }
    bail!(
        "could not choose a unique temporary filename in {}",
        directory.display()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Write;

    use super::*;

    #[test]
    fn session_ids_are_compact_lowercase_crockford_values() {
        let mut seen = HashSet::new();
        for _ in 0..128 {
            let id = session_id().unwrap();
            assert_eq!(id.len(), 12);
            assert!(id.starts_with("ses_"));
            assert!(id[4..].bytes().all(|byte| CROCKFORD.contains(&byte)));
            seen.insert(id);
        }
        // Collision handling belongs to the database insert. This only guards
        // against a broken source that returns one constant formatted value.
        assert!(seen.len() > 120);
    }

    #[cfg(unix)]
    #[test]
    fn temporary_files_are_exclusive_private_and_distinct() {
        use std::os::unix::fs::MetadataExt;

        let directory =
            std::env::temp_dir().join(format!("mu-random-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (mut first, first_path) = create_temp_file(&directory, "spill-", ".tmp").unwrap();
        let (_, second_path) = create_temp_file(&directory, "spill-", ".tmp").unwrap();
        first.write_all(b"content").unwrap();
        first.sync_all().unwrap();

        assert_ne!(first_path, second_path);
        assert_eq!(std::fs::metadata(&first_path).unwrap().mode() & 0o077, 0);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"content");
        let _ = std::fs::remove_dir_all(directory);
    }
}
