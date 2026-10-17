//! Explicit local photograph loading and validation.
//!
//! `Photo` keeps a validated JPEG or PNG in memory for one session. It never
//! copies the source file into the repository or writes a derived image.

use std::{
    fmt,
    fs::{self, File},
    io::{self, Cursor, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use image::{ImageFormat, ImageReader, Limits};
use thiserror::Error;

/// The largest source image the prototype will read from disk.
pub const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODE_ALLOCATION: u64 = 64 * 1024 * 1024;

/// A fully validated in-memory photograph available to the current scene.
#[derive(Clone, PartialEq, Eq)]
pub struct Photo {
    path: PathBuf,
    format: PhotoFormat,
    bytes: Arc<[u8]>,
}

impl Photo {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PhotoError> {
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|source| PhotoError::Inspect {
            path: path.to_owned(),
            source,
        })?;

        if metadata.is_dir() {
            return Err(PhotoError::Directory(path.to_owned()));
        }

        if !metadata.is_file() {
            return Err(PhotoError::NotARegularFile(path.to_owned()));
        }

        let bytes = read_limited(path)?;
        let image_format =
            image::guess_format(&bytes).map_err(|source| PhotoError::Unrecognized {
                path: path.to_owned(),
                source,
            })?;
        let format = PhotoFormat::from_image_format(image_format)
            .ok_or_else(|| PhotoError::Unsupported(path.to_owned()))?;

        validate(&bytes, image_format, path, format)?;

        Ok(Self {
            path: path.to_owned(),
            format,
            bytes: Arc::from(bytes),
        })
    }

    /// The original, validated bytes to encode only for an Ollama request.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> PhotoFormat {
        self.format
    }

    #[cfg(test)]
    pub(crate) fn test_photo(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            format: PhotoFormat::Png,
            bytes: Arc::from([0_u8, 1, 2, 3].as_slice()),
        }
    }
}

impl fmt::Debug for Photo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Photo")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

/// The only formats this prototype intentionally accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoFormat {
    Jpeg,
    Png,
}

impl PhotoFormat {
    fn from_image_format(format: ImageFormat) -> Option<Self> {
        match format {
            ImageFormat::Jpeg => Some(Self::Jpeg),
            ImageFormat::Png => Some(Self::Png),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
        }
    }
}

fn read_limited(path: &Path) -> Result<Vec<u8>, PhotoError> {
    let file = File::open(path).map_err(|source| PhotoError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = file.take((MAX_PHOTO_BYTES + 1) as u64);
    let mut bytes = Vec::with_capacity(MAX_PHOTO_BYTES.min(64 * 1024));

    reader
        .read_to_end(&mut bytes)
        .map_err(|source| PhotoError::Read {
            path: path.to_owned(),
            source,
        })?;

    if bytes.len() > MAX_PHOTO_BYTES {
        return Err(PhotoError::TooLarge(path.to_owned()));
    }

    Ok(bytes)
}

fn validate(
    bytes: &[u8],
    image_format: ImageFormat,
    path: &Path,
    format: PhotoFormat,
) -> Result<(), PhotoError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);

    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format);
    reader.limits(limits);
    reader
        .decode()
        .map(|_| ())
        .map_err(|source| PhotoError::Invalid {
            path: path.to_owned(),
            format: format.name(),
            source,
        })
}

#[derive(Debug, Error)]
pub enum PhotoError {
    #[error("could not inspect photo {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("photo path is a directory: {0}")]
    Directory(PathBuf),

    #[error("photo path is not a regular file: {0}")]
    NotARegularFile(PathBuf),

    #[error("could not read photo {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("photo is larger than the 10 MiB limit: {0}")]
    TooLarge(PathBuf),

    #[error("photo is not a recognized image: {path}: {source}")]
    Unrecognized {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },

    #[error("photo must be a JPEG or PNG: {0}")]
    Unsupported(PathBuf),

    #[error("photo is not a valid {format} image: {path}: {source}")]
    Invalid {
        path: PathBuf,
        format: &'static str,
        #[source]
        source: image::ImageError,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Cursor, Write},
    };

    use image::{DynamicImage, ImageFormat};
    use tempfile::tempdir;

    use super::{MAX_PHOTO_BYTES, Photo, PhotoError, PhotoFormat};

    fn tiny_png() -> Vec<u8> {
        let image = DynamicImage::new_rgba8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn validates_a_png_by_contents_not_filename_extension() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("tree.jpg");
        fs::write(&path, tiny_png()).unwrap();

        let photo = Photo::load(&path).unwrap();

        assert_eq!(photo.path(), path);
        assert_eq!(photo.format(), PhotoFormat::Png);
        assert!(!photo.bytes().is_empty());
    }

    #[test]
    fn rejects_missing_unsupported_and_oversized_inputs() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing.png");
        assert!(matches!(
            Photo::load(missing),
            Err(PhotoError::Inspect { .. })
        ));

        let unsupported = directory.path().join("not-an-image.txt");
        fs::write(&unsupported, "definitely not an image").unwrap();
        assert!(matches!(
            Photo::load(unsupported),
            Err(PhotoError::Unrecognized { .. })
        ));

        let oversized = directory.path().join("large.png");
        let mut file = fs::File::create(&oversized).unwrap();
        file.write_all(&vec![0; MAX_PHOTO_BYTES + 1]).unwrap();
        assert!(matches!(
            Photo::load(oversized),
            Err(PhotoError::TooLarge(_))
        ));
    }
}
