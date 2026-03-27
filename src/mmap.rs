//! Safe memory-mapped file wrapper.
//!
//! Provides [`MappedFile`], a safe abstraction over [`memmap2::Mmap`] that
//! encapsulates the `unsafe` mmap call so that downstream crates with
//! `#![deny(unsafe_code)]` can memory-map files without writing any unsafe code
//! themselves.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;

/// A read-only memory-mapped file.
///
/// The mapped region remains valid for the lifetime of this struct.  The
/// underlying file handle is kept open to satisfy OS-level requirements on some
/// platforms.
///
/// # Safety note
///
/// This type wraps [`memmap2::Mmap`] which is inherently `unsafe` because the
/// OS allows other processes to modify or truncate the backing file while the
/// mapping is alive.  This wrapper exposes safe constructors as a practical
/// tradeoff (consistent with common Rust mmap wrappers): the mapped bytes are
/// treated as an immutable `&[u8]`, but **external modification of the file by
/// another process can cause undefined behaviour**.  Callers must ensure no
/// other process writes to or truncates the file while the `MappedFile` is
/// alive (e.g. via file locks or workflow conventions).
///
/// # Example
///
/// ```no_run
/// use rebgzf::MappedFile;
///
/// let mapped = MappedFile::open("data.gz")?;
/// let first_two = &mapped[..2]; // gzip magic bytes
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct MappedFile {
    mmap: memmap2::Mmap,
    /// Kept open so the mapping remains valid on all platforms.
    _file: File,
}

impl MappedFile {
    /// Memory-map a file in read-only mode.
    ///
    /// Returns an error if the file cannot be opened, is not a regular file, or
    /// has zero length.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path.as_ref())?;
        Self::from_file(file)
    }

    /// Memory-map an already-open file.
    ///
    /// Returns `Ok(MappedFile)` on success. If the file is not a regular file
    /// or is empty, returns `Err((io::Error, File))` so the caller can reuse
    /// the file handle (e.g. for streaming fallback) without re-opening it.
    /// Other I/O errors (e.g. mmap failure) are returned with the file handle
    /// as well.
    pub fn try_from_file(file: File) -> std::result::Result<Self, (io::Error, File)> {
        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => return Err((e, file)),
        };

        if !metadata.is_file() {
            return Err((
                io::Error::new(io::ErrorKind::InvalidInput, "MappedFile requires a regular file"),
                file,
            ));
        }
        if metadata.len() == 0 {
            return Err((
                io::Error::new(io::ErrorKind::InvalidInput, "MappedFile requires a non-empty file"),
                file,
            ));
        }

        // Safety: we keep the file open for the lifetime of the mapping and
        // never write through it.  External modification of the backing file
        // is the caller's responsibility to prevent (see struct-level docs).
        let mmap = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(m) => m,
            Err(e) => return Err((e, file)),
        };

        Ok(Self { mmap, _file: file })
    }

    /// Memory-map an already-open file, consuming it on success.
    ///
    /// Returns an error if the file is not a regular file or has zero length.
    /// Use [`try_from_file`](Self::try_from_file) when you need the file handle
    /// back on failure.
    pub fn from_file(file: File) -> io::Result<Self> {
        Self::try_from_file(file).map_err(|(e, _)| e)
    }

    /// Advise the OS that the mapped region will be accessed sequentially.
    ///
    /// This is a best-effort hint; failure is non-fatal.  Only available on
    /// Unix platforms.
    #[cfg(unix)]
    pub fn advise_sequential(&self) -> io::Result<()> {
        self.mmap.advise(memmap2::Advice::Sequential)
    }

    /// Returns the length of the mapped region in bytes.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Returns `true` if the mapped region is empty.
    ///
    /// This always returns `false` because [`open`](Self::open) rejects
    /// zero-length files.
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.mmap
    }
}

impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        &self.mmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_open_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"hello").unwrap();
        }

        let mapped = MappedFile::open(&path).unwrap();
        assert_eq!(mapped.len(), 5);
        assert!(!mapped.is_empty());
        assert_eq!(&*mapped, b"hello");
        assert_eq!(mapped.as_ref(), b"hello");
    }

    #[test]
    fn test_open_empty_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        File::create(&path).unwrap();

        let err = MappedFile::open(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_open_nonexistent_fails() {
        let err = MappedFile::open("/nonexistent/path").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
