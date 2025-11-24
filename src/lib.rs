//! This library provides both high level readers and writers for the BGZF format as well as lower level
//! compressor and decompressor functions.
//!
//! Bgzf is a multi-gzip format that adds an extra field to the header indicating how large the
//! complete block (with header and footer) is.
//!
//! # Examples
//!
//! ```rust
//! use bgzf::{Reader, Writer};
//! use std::error::Error;
//! use std::io;
//!
//! /// Contrived example that decompresses stdin and compresses to stdout.
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let mut reader = Reader::new(io::stdin());
//!     let mut writer = Writer::new(io::stdout(), 2.try_into()?);
//!     let total_bytes = io::copy(&mut reader, &mut writer)?;
//!     eprintln!("{} uncompressed bytes", total_bytes);
//!     Ok(())
//! }
//! ```
#![forbid(unsafe_code)]
#![allow(clippy::must_use_candidate, clippy::missing_errors_doc, clippy::missing_panics_doc)]

// Re-export the reader and writer to the same level.
mod reader;
mod writer;
pub use reader::*;
pub use writer::*;

use std::io;

use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use libdeflater::CompressionLvl;
use thiserror::Error;

/// The maximum uncompressed blocksize for BGZF compression (taken from bgzip), used for initializing blocks.
pub const BGZF_BLOCK_SIZE: usize = 65280;

/// 128 KB default buffer size, same as pigz.
pub const BUFSIZE: usize = 128 * 1024;

/// Default from bgzf: compress(BGZF_BLOCK_SIZE) < BGZF_MAX_BLOCK_SIZE
/// 65536 which is u16::MAX + 1
pub(crate) const MAX_BGZF_BLOCK_SIZE: usize = 64 * 1024;

pub(crate) static BGZF_EOF: &[u8] = &[
    0x1f, 0x8b, // ID1, ID2
    0x08, // CM = DEFLATE
    0x04, // FLG = FEXTRA
    0x00, 0x00, 0x00, 0x00, // MTIME = 0
    0x00, // XFL = 0
    0xff, // OS = 255 (unknown)
    0x06, 0x00, // XLEN = 6
    0x42, 0x43, // SI1, SI2
    0x02, 0x00, // SLEN = 2
    0x1b, 0x00, // BSIZE = 27
    0x03, 0x00, // CDATA
    0x00, 0x00, 0x00, 0x00, // CRC32 = 0x00000000
    0x00, 0x00, 0x00, 0x00, // ISIZE = 0
];

pub(crate) const BGZF_HEADER_SIZE: usize = 18;
pub(crate) const BGZF_FOOTER_SIZE: usize = 8;
pub(crate) const BGZF_MAGIC_BYTE_A: u8 = 31;
pub(crate) const BGZF_MAGIC_BYTE_B: u8 = 139;
pub(crate) const BGZF_COMPRESSION_METHOD: u8 = 8;
pub(crate) const BGZF_NAME_COMMENT_EXTRA_FLAG: u8 = 4;
pub(crate) const BGZF_DEFAULT_MTIME: u32 = 0;
pub(crate) const BGZF_DEFAULT_OS: u8 = 255;
pub(crate) const BGZF_EXTRA_FLAG_LEN: u16 = 6;
pub(crate) const BGZF_SUBFIELD_ID1: u8 = b'B';
pub(crate) const BGZF_SUBFIELD_ID2: u8 = b'C';
pub(crate) const BGZF_SUBFIELD_LEN: u16 = 2;
pub(crate) const BGZF_BLOCK_SIZE_OFFSET: usize = 16;

pub(crate) const BGZF_COMPRESSION_HINT_BEST: u8 = 2;
pub(crate) const BGZF_COMPRESSION_HINT_FASTEST: u8 = 4;
pub(crate) const BGZF_COMPRESSION_HINT_OTHER: u8 = 0;

const EXTRA: f64 = 0.1;

/// Add 10% of the size of the input data to the size of the output amount to account for
/// compression levels that actually increase the output datasize for some inputs (i.e totally
/// random input data).
#[inline]
fn extra_amount(input_len: usize) -> usize {
    std::cmp::max(128, (input_len as f64 * EXTRA) as usize)
}

type BgzfResult<T> = Result<T, BgzfError>;

#[non_exhaustive]
#[derive(Error, Debug)]
pub enum BgzfError {
    #[error("Compressed block size ({0}) exceeds max allowed: ({1})")]
    BlockSizeExceeded(usize, usize),
    #[error("Invalid compression level: {0}")]
    CompressionLevel(u8),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Invalid checksum, found {found}, expected {expected}")]
    InvalidChecksum { found: u32, expected: u32 },
    #[error("Invalid block header: {0}")]
    InvalidHeader(&'static str),
    #[error("LibDeflater compression error: {0:?}")]
    LibDeflaterCompress(libdeflater::CompressionError),
    #[error(transparent)]
    LibDelfaterDecompress(#[from] libdeflater::DecompressionError),
}

/// The expected checksum and number of bytes for decompressed data.
#[derive(Debug, Copy, Clone)]
struct ChecksumValues {
    /// The check sum
    sum: u32,
    /// The number of bytes that went into the sum
    amount: u32,
}

/// Level of compression to use for for the compressors.
///
/// Valid values are 1-12. See [libdeflater](https://github.com/ebiggers/libdeflate#compression-levels) documentation on levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionLevel(CompressionLvl);

#[allow(dead_code)]
impl CompressionLevel {
    /// Create a new [`CompressionLevel`] instance.
    ///
    /// Valid levels are 1-12.
    #[allow(clippy::cast_lossless)]
    pub fn new(level: u8) -> BgzfResult<Self> {
        // libdeflater::CompressionLvlError contains no information
        Ok(Self(
            CompressionLvl::new(level as i32).map_err(|_e| BgzfError::CompressionLevel(level))?,
        ))
    }

    /// Get the inner compression level
    fn inner(&self) -> &libdeflater::CompressionLvl {
        &self.0
    }
}

impl TryFrom<u8> for CompressionLevel {
    type Error = BgzfError;

    /// Try to convert a `u8` to a compression level.
    ///
    /// # Example
    /// ```rust
    /// use bgzf::CompressionLevel;
    ///
    /// let level: CompressionLevel = 2.try_into().unwrap();
    /// assert_eq!(level, CompressionLevel::new(2).unwrap());
    /// ```
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CompressionLevel> for u8 {
    /// Convenience method vor converting [`CompressionLevel`] back to a [`u8`].
    fn from(level: CompressionLevel) -> Self {
        let inner: i32 = level.inner().into();
        inner as u8
    }
}

impl From<&CompressionLevel> for u8 {
    /// Convenience method vor converting [`CompressionLevel`] back to a [`u8`].
    fn from(level: &CompressionLevel) -> Self {
        let inner: i32 = level.inner().into();
        inner as u8
    }
}

/// [`Compressor`] will BGZF compress a block of bytes with the [`Compressor::compress`] method, allowing for reuse of the compressor itself.
///
/// # Example
///
/// ```rust
/// use bgzf::{Compressor, CompressionLevel};
///
/// let mut compressor = Compressor::new(2.try_into().unwrap());
/// let input = &[b'A'; 100];
/// let mut output_buffer = vec![];
/// compressor.compress(input, &mut output_buffer).unwrap();
/// assert!(input.len() > output_buffer.len());
/// ```
pub struct Compressor {
    inner: libdeflater::Compressor,
    level: CompressionLevel,
}

#[allow(dead_code)]
impl Compressor {
    /// Create a new [`Compressor`] with the given [`CompressionLevel`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use bgzf::Compressor;
    /// let compressor = Compressor::new(3.try_into().expect("Invalid compression level"));
    /// ```
    pub fn new(level: CompressionLevel) -> Self {
        Self { inner: libdeflater::Compressor::new(*level.inner()), level }
    }

    #[inline]
    fn inner(&self) -> &libdeflater::Compressor {
        &self.inner
    }

    #[inline]
    fn inner_mut(&mut self) -> &mut libdeflater::Compressor {
        &mut self.inner
    }

    /// Compress a block of bytes, adding a header and footer.
    #[inline]
    pub fn compress(&mut self, input: &[u8], buffer: &mut Vec<u8>) -> BgzfResult<()> {
        buffer.resize_with(
            BGZF_HEADER_SIZE + input.len() + extra_amount(input.len()) + BGZF_FOOTER_SIZE,
            || 0,
        );

        let bytes_written = self
            .inner_mut()
            .deflate_compress(input, &mut buffer[BGZF_HEADER_SIZE..])
            .map_err(BgzfError::LibDeflaterCompress)?;

        // Make sure that compressed buffer is smaller than
        if bytes_written >= MAX_BGZF_BLOCK_SIZE {
            return Err(BgzfError::BlockSizeExceeded(bytes_written, MAX_BGZF_BLOCK_SIZE));
        }
        let mut check = libdeflater::Crc::new();
        check.update(input);

        // Add header with total byte sizes
        let header = header_inner(self.level, bytes_written as u16);
        buffer[0..BGZF_HEADER_SIZE].copy_from_slice(&header);
        buffer.truncate(BGZF_HEADER_SIZE + bytes_written);

        buffer.write_u32::<LittleEndian>(check.sum())?;
        buffer.write_u32::<LittleEndian>(input.len() as u32)?;

        Ok(())
    }

    /// Append the EOF block.
    pub fn append_eof(bytes: &mut Vec<u8>) {
        bytes.extend(BGZF_EOF);
    }
}

/// [`Decompressor`] will decompress a BGZF block.
struct Decompressor(libdeflater::Decompressor);

#[allow(dead_code)]
impl Decompressor {
    /// Create a new [`Decompressor`].
    fn new() -> Self {
        Self(libdeflater::Decompressor::new())
    }

    #[inline]
    fn inner(&self) -> &libdeflater::Decompressor {
        &self.0
    }

    #[inline]
    fn inner_mut(&mut self) -> &mut libdeflater::Decompressor {
        &mut self.0
    }

    /// Decompress a block of bytes.
    ///
    /// This expects the `output` to be the exact size needed to hold the decompressed input.
    /// This expects the input slice to have the header and footer values removed.
    #[inline]
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        checksum_values: ChecksumValues,
    ) -> BgzfResult<()> {
        if checksum_values.amount != 0 {
            let _bytes_decompressed = self.inner_mut().deflate_decompress(input, output)?;
        }
        let mut new_check = libdeflater::Crc::new();
        new_check.update(output);

        if checksum_values.sum != new_check.sum() {
            return Err(BgzfError::InvalidChecksum {
                found: new_check.sum(),
                expected: checksum_values.sum,
            });
        }
        Ok(())
    }
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

/// Create an Bgzf style header.
#[inline]
fn header_inner(
    compression_level: CompressionLevel,
    compressed_size: u16,
) -> [u8; BGZF_HEADER_SIZE] {
    // Determine hint to place in header
    // From https://github.com/rust-lang/flate2-rs/blob/b2e976da21c18c8f31132e93a7f803b5e32f2b6d/src/gz/mod.rs#L235
    let comp_value = if compression_level.inner() >= &CompressionLvl::best() {
        BGZF_COMPRESSION_HINT_BEST
    } else if compression_level.inner() <= &CompressionLvl::fastest() {
        BGZF_COMPRESSION_HINT_FASTEST
    } else {
        BGZF_COMPRESSION_HINT_OTHER
    };

    let mut header = [0u8; BGZF_HEADER_SIZE];
    let mut cursor = std::io::Cursor::new(&mut header[..]);
    cursor.write_u8(BGZF_MAGIC_BYTE_A).unwrap(); // magic byte
    cursor.write_u8(BGZF_MAGIC_BYTE_B).unwrap(); // magic byte
    cursor.write_u8(BGZF_COMPRESSION_METHOD).unwrap(); // compression method
    cursor.write_u8(BGZF_NAME_COMMENT_EXTRA_FLAG).unwrap(); // name / comment / extraflag
    cursor.write_u32::<LittleEndian>(BGZF_DEFAULT_MTIME).unwrap(); // mtime
    cursor.write_u8(comp_value).unwrap(); // compression value
    cursor.write_u8(BGZF_DEFAULT_OS).unwrap(); // OS
    cursor.write_u16::<LittleEndian>(BGZF_EXTRA_FLAG_LEN).unwrap(); // Extra flag len
    cursor.write_u8(BGZF_SUBFIELD_ID1).unwrap(); // Bgzf subfield ID 1
    cursor.write_u8(BGZF_SUBFIELD_ID2).unwrap(); // Bgzf subfield ID2
    cursor.write_u16::<LittleEndian>(BGZF_SUBFIELD_LEN).unwrap(); // Bgzf subfield len
    cursor
        .write_u16::<LittleEndian>(
            compressed_size + BGZF_HEADER_SIZE as u16 + BGZF_FOOTER_SIZE as u16 - 1,
        )
        .unwrap(); // Size of block including header and footer - 1 BLEN

    header
}

/// Check that the header is as expected for this format
#[inline]
fn check_header(bytes: &[u8]) -> BgzfResult<()> {
    // Check that the extra field flag is set
    if bytes[3] & 4 != BGZF_NAME_COMMENT_EXTRA_FLAG {
        Err(BgzfError::InvalidHeader("Extra field flag not set"))
    } else if bytes[12] != BGZF_SUBFIELD_ID1 || bytes[13] != BGZF_SUBFIELD_ID2 {
        // Check for BC in SID
        Err(BgzfError::InvalidHeader("Bad SID"))
    } else {
        Ok(())
    }
}

/// Extract the block size from the header.
#[inline]
fn get_block_size(bytes: &[u8]) -> usize {
    LittleEndian::read_u16(&bytes[BGZF_BLOCK_SIZE_OFFSET..]) as usize + 1
}

/// Get the expected uncompressed size and check sum from the footer
#[inline]
fn get_footer_values(input: &[u8]) -> ChecksumValues {
    let check_sum = LittleEndian::read_u32(&input[input.len() - 8..input.len() - 4]);
    let check_amount = LittleEndian::read_u32(&input[input.len() - 4..]);
    ChecksumValues { sum: check_sum, amount: check_amount }
}

/// Strip the footer off of a compressed block.
#[inline]
fn strip_footer(input: &[u8]) -> &[u8] {
    &input[..input.len() - BGZF_FOOTER_SIZE]
}

#[cfg(test)]
mod test {
    use std::io::{Read, Write};
    use std::{
        fs::File,
        io::{BufReader, BufWriter},
    };

    use proptest::prelude::*;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_simple_bgzfsync() {
        let dir = tempdir().unwrap();

        // Define and write input bytes
        let input = b"
        This is a longer test than normal to come up with a bunch of text.
        We'll read just a few lines at a time.
        What if this is a longer string, does that then make
        things fail?
        ";

        let orig_file = dir.path().join("orig.output.txt");
        let mut orig_writer = BufWriter::new(File::create(&orig_file).unwrap());
        orig_writer.write_all(input).unwrap();
        drop(orig_writer);

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());

        // Compress input to output
        let mut bgzf = Writer::new(out_writer, CompressionLevel::new(3).unwrap());
        bgzf.write_all(input).unwrap();
        bgzf.flush().unwrap();
        drop(bgzf);

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut decoder = Reader::new(&result[..]);
        let mut bytes = vec![];
        decoder.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
    }

    const DICT_SIZE: usize = 32768;
    proptest! {
        #[test]
        fn proptest_bgzf(
            input in prop::collection::vec(0..u8::MAX, 1..(DICT_SIZE * 10)),
            buf_size in DICT_SIZE..BGZF_BLOCK_SIZE,
            write_size in 1..BGZF_BLOCK_SIZE * 4,
            comp_level in 1..12_u8
        ) {
            let dir = tempdir().unwrap();

            // Create output file
            let output_file = dir.path().join("output.txt");
            let out_writer = BufWriter::new(File::create(&output_file).unwrap());

            // Compress input to output
            let mut writer = Writer::with_capacity(out_writer, CompressionLevel::new(comp_level).unwrap(), buf_size);

            for chunk in input.chunks(write_size) {
                writer.write_all(chunk).unwrap();
            }
            writer.flush().unwrap();
            drop(writer);

            // Read output back in
            let mut reader = BufReader::new(File::open(output_file).unwrap());
            let mut result = vec![];
            reader.read_to_end(&mut result).unwrap();

            // Decompress it
            let mut gz = Reader::new(&result[..]);
            let mut bytes = vec![];
            gz.read_to_end(&mut bytes).unwrap();

            // Assert decompressed output is equal to input
            assert_eq!(input.clone(), bytes);
        }
    }
}
