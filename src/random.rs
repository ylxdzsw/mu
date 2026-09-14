use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Read bytes from the Windows system cryptographic random source.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    let length = u32::try_from(N).context("random request is too large")?;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        bail!("Windows random source failed with NTSTATUS {status:#x}");
    }
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

fn random_hex<const N: usize>() -> Result<String> {
    Ok(random_bytes::<N>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Create a private, unpredictably named file in `directory`.
///
/// `create_new` is the important part: a random name is only a hint until the
/// kernel performs the no-replace create atomically.
pub fn create_temp_file(directory: &Path, prefix: &str, suffix: &str) -> Result<(File, PathBuf)> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating temporary directory {}", directory.display()))?;
    for _ in 0..32 {
        let token = random_hex::<12>()?;
        let path = directory.join(format!("{prefix}{token}{suffix}"));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
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

/// Create a private, unpredictably named directory in `directory`.
#[cfg(test)]
pub fn create_temp_dir(directory: &Path, prefix: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating temporary directory {}", directory.display()))?;
    let builder = std::fs::DirBuilder::new();
    for _ in 0..32 {
        let path = directory.join(format!("{prefix}{}", random_hex::<12>()?));
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary directory {}", path.display()));
            }
        }
    }
    bail!(
        "could not choose a unique temporary directory in {}",
        directory.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_compact_lowercase_crockford_values() {
        let id = session_id().unwrap();
        assert_eq!(id.len(), 12);
        assert!(id.starts_with("ses_"));
        assert!(id[4..].bytes().all(|byte| CROCKFORD.contains(&byte)));
    }

    #[test]
    fn temporary_paths_are_private_unique_and_use_the_requested_name_shape() {
        let directory = create_temp_dir(&std::env::temp_dir(), "mu-random-test-").unwrap();
        let other = create_temp_dir(&std::env::temp_dir(), "mu-random-test-").unwrap();
        let (_, path) = create_temp_file(&directory, "spill-", ".tmp").unwrap();

        assert_ne!(directory, other);
        assert!(
            directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mu-random-test-")
        );
        assert_eq!(path.parent(), Some(directory.as_path()));
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("spill-") && name.ends_with(".tmp"));
        let _ = std::fs::remove_dir_all(directory);
        let _ = std::fs::remove_dir_all(other);
    }
}
