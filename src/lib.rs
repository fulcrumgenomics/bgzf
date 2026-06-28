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
#![deny(unsafe_code)]
#![allow(clippy::must_use_candidate, clippy::missing_errors_doc, clippy::missing_panics_doc)]

// Re-export the reader and writer to the same level.
mod reader;
mod writer;
pub use reader::*;
pub use writer::*;

use std::io;

use byteorder::{ByteOrder, LittleEndian};
use libdeflater::CompressionLvl;
use thiserror::Error;

/// Buffer operations that avoid unnecessary memory initialization.
mod buffer_ops {
    /// Resizes a buffer to `new_len` without initializing the new bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all bytes in `0..new_len` are written
    /// before any of them are read. This is safe because:
    /// - `u8` has no invalid bit patterns
    /// - `reserve_exact()` ensures sufficient capacity
    /// - The buffer is cleared first, so no stale data remains
    #[inline(always)]
    #[allow(unsafe_code, clippy::uninit_vec)]
    pub(crate) unsafe fn resize_uninit(buffer: &mut Vec<u8>, new_len: usize) {
        buffer.clear();
        buffer.reserve_exact(new_len);
        buffer.set_len(new_len);
    }
}

/// The maximum uncompressed blocksize for BGZF compression (taken from bgzip), used for initializing blocks.
pub const BGZF_BLOCK_SIZE: usize = 65280;

/// 128 KB default buffer size, same as pigz.
pub const BUFSIZE: usize = 128 * 1024;

/// The maximum size, in bytes, of a complete BGZF block (header + payload + footer); the on-disk
/// `BSIZE` field is this minus one, so it must fit in a `u16` (65536 = `u16::MAX` + 1). This is also
/// the largest uncompressed size any single block can hold, so the reader sizes its decompression
/// buffer to it and rejects blocks whose ISIZE claims more (see [`BgzfError::UncompressedSizeExceeded`]).
/// Matches htslib's `BGZF_MAX_BLOCK_SIZE`.
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
/// Size of a DEFLATE stored-block header: 1 byte (BFINAL/BTYPE) + LEN (u16 LE) + NLEN (u16 LE).
pub(crate) const DEFLATE_STORED_HEADER_SIZE: usize = 5;
pub(crate) const BGZF_SIZEOF_CRC32: usize = 4;
pub(crate) const BGZF_NAME_COMMENT_EXTRA_FLAG: u8 = 4;
pub(crate) const BGZF_SUBFIELD_ID1: u8 = b'B';
pub(crate) const BGZF_SUBFIELD_ID2: u8 = b'C';
pub(crate) const BGZF_BLOCK_SIZE_OFFSET: usize = 16;
pub(crate) const BGZF_XFL_OFFSET: usize = 8;

pub(crate) const BGZF_COMPRESSION_HINT_BEST: u8 = 2;
pub(crate) const BGZF_COMPRESSION_HINT_FASTEST: u8 = 4;
pub(crate) const BGZF_COMPRESSION_HINT_OTHER: u8 = 0;

/// Pre-computed BGZF header template. Only bytes 8 (XFL) and 16-17 (BSIZE) vary.
const HEADER_TEMPLATE: [u8; BGZF_HEADER_SIZE] = [
    0x1f, 0x8b, // ID1, ID2 (magic)
    0x08, // CM = DEFLATE
    0x04, // FLG = FEXTRA
    0x00, 0x00, 0x00, 0x00, // MTIME = 0
    0x00, // XFL = placeholder (byte 8)
    0xff, // OS = 255
    0x06, 0x00, // XLEN = 6
    b'B', b'C', // SI1, SI2
    0x02, 0x00, // SLEN = 2
    0x00, 0x00, // BSIZE placeholder (bytes 16-17)
];

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
    #[error("Uncompressed block size ({found}) exceeds maximum ({max})")]
    UncompressedSizeExceeded { found: usize, max: usize },
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
/// Valid values are 0-12, where 0 stores the data uncompressed (DEFLATE stored blocks) and is the
/// fastest to both write and read. Levels 1-12 invoke libdeflate; see its
/// [documentation](https://github.com/ebiggers/libdeflate#compression-levels) on levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionLevel(CompressionLvl);

#[allow(dead_code)]
impl CompressionLevel {
    /// Create a new [`CompressionLevel`] instance.
    ///
    /// Valid levels are 0-12; level 0 stores the data uncompressed.
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
    #[must_use]
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
    #[inline(always)]
    pub fn compress(&mut self, input: &[u8], buffer: &mut Vec<u8>) -> BgzfResult<()> {
        // Use libdeflate's official bound calculation
        let compress_bound = self.inner_mut().deflate_compress_bound(input.len());
        let required_size = BGZF_HEADER_SIZE + compress_bound + BGZF_FOOTER_SIZE;

        // SAFETY: All bytes in 0..final_len are written before the function returns:
        // - bytes 0..18: header via copy_from_slice
        // - bytes 18..18+bytes_written: written by deflate_compress
        // - bytes footer_offset..footer_offset+8: footer via copy_from_slice
        // - buffer is truncated to final_len, removing any uninitialized trailing bytes
        #[allow(unsafe_code)]
        unsafe {
            buffer_ops::resize_uninit(buffer, required_size);
        }

        let bytes_written = self
            .inner_mut()
            .deflate_compress(input, &mut buffer[BGZF_HEADER_SIZE..])
            .map_err(BgzfError::LibDeflaterCompress)?;

        if bytes_written >= MAX_BGZF_BLOCK_SIZE {
            return Err(BgzfError::BlockSizeExceeded(bytes_written, MAX_BGZF_BLOCK_SIZE));
        }

        // Write header
        let header = header_inner(self.level, bytes_written as u16);
        buffer[0..BGZF_HEADER_SIZE].copy_from_slice(&header);

        // Write footer directly at computed offset
        let footer_offset = BGZF_HEADER_SIZE + bytes_written;
        buffer[footer_offset..footer_offset + BGZF_SIZEOF_CRC32]
            .copy_from_slice(&crc32(input).to_le_bytes());
        buffer[footer_offset + BGZF_SIZEOF_CRC32..footer_offset + BGZF_FOOTER_SIZE]
            .copy_from_slice(&(input.len() as u32).to_le_bytes());

        // Truncate to final size (removes uninitialized bytes beyond footer)
        buffer.truncate(footer_offset + BGZF_FOOTER_SIZE);

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
        // Only the bytes `deflate_decompress` actually writes belong to this block; a corrupt block
        // can produce fewer than the footer's ISIZE, and `output` may still hold stale bytes from a
        // previous, larger block. Checksum just the written prefix (and reject a short block) rather
        // than the whole `output`.
        let decompressed = if checksum_values.amount != 0 {
            self.inner_mut().deflate_decompress(input, output)?
        } else {
            0
        };

        let found = crc32(&output[..decompressed]);
        if decompressed != output.len() || found != checksum_values.sum {
            return Err(BgzfError::InvalidChecksum { found, expected: checksum_values.sum });
        }
        Ok(())
    }
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a BGZF header with the given compression level and compressed size.
#[inline(always)]
fn header_inner(
    compression_level: CompressionLevel,
    compressed_size: u16,
) -> [u8; BGZF_HEADER_SIZE] {
    let mut header = HEADER_TEMPLATE;

    // Patch XFL (compression hint)
    header[BGZF_XFL_OFFSET] = if compression_level.inner() >= &CompressionLvl::best() {
        BGZF_COMPRESSION_HINT_BEST
    } else if compression_level.inner() <= &CompressionLvl::fastest() {
        BGZF_COMPRESSION_HINT_FASTEST
    } else {
        BGZF_COMPRESSION_HINT_OTHER
    };

    // Patch BSIZE (little-endian u16)
    let bsize = compressed_size + BGZF_HEADER_SIZE as u16 + BGZF_FOOTER_SIZE as u16 - 1;
    header[BGZF_BLOCK_SIZE_OFFSET..BGZF_BLOCK_SIZE_OFFSET + 2]
        .copy_from_slice(&bsize.to_le_bytes());

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

/// Compute the gzip/BGZF CRC32 of `data`.
#[inline]
fn crc32(data: &[u8]) -> u32 {
    let mut crc = libdeflater::Crc::new();
    crc.update(data);
    crc.sum()
}

/// If `deflate_header` (the start of a DEFLATE stream) begins a single, final *stored*
/// (uncompressed) block, return its declared length (`LEN`); otherwise return `None`.
///
/// BGZF written at compression level 0 — and the "no compression" mode of other tools — encodes
/// each block this way. The caller still confirms that `LEN` matches the block's framing, CRC and
/// uncompressed size before trusting it; any other shape (real compressed data, the empty EOF
/// block, a non-final or multi-block stream, or a corrupt length field) returns `None` and is left
/// to the normal decompression path.
#[inline]
fn stored_block_len(deflate_header: &[u8]) -> Option<usize> {
    // The header is a 1-byte field (BFINAL in bit 0, BTYPE in bits 1-2) followed by LEN and NLEN
    // (u16 little-endian, NLEN = !LEN). A final stored block has BFINAL=1, BTYPE=00, i.e. low three
    // bits 0b001.
    if deflate_header.len() < DEFLATE_STORED_HEADER_SIZE || deflate_header[0] & 0b0000_0111 != 0b001
    {
        return None;
    }
    let len = u16::from_le_bytes([deflate_header[1], deflate_header[2]]);
    let nlen = u16::from_le_bytes([deflate_header[3], deflate_header[4]]);
    (nlen == !len).then_some(len as usize)
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

    /// Test that the EOF marker is written exactly once at the end of the output
    /// when using finish().
    #[test]
    fn test_eof_marker_written_once_with_finish() {
        // Test with data that doesn't fill a complete block
        let mut output = Vec::new();
        {
            let mut writer = Writer::new(&mut output, CompressionLevel::new(3).unwrap());
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        }

        // Verify EOF marker appears exactly once at the end
        assert!(output.ends_with(BGZF_EOF), "Output should end with BGZF_EOF marker");

        // Count occurrences of EOF marker
        let eof_count = output.windows(BGZF_EOF.len()).filter(|w| *w == BGZF_EOF).count();
        assert_eq!(eof_count, 1, "EOF marker should appear exactly once");
    }

    /// Test that EOF marker is written exactly once when relying on Drop.
    #[test]
    fn test_eof_marker_written_once_on_drop() {
        let mut output = Vec::new();
        {
            let mut writer = Writer::new(&mut output, CompressionLevel::new(3).unwrap());
            writer.write_all(b"hello").unwrap();
            // Don't call finish(), let Drop handle it
        }

        // Verify EOF marker appears exactly once at the end
        assert!(output.ends_with(BGZF_EOF), "Output should end with BGZF_EOF marker");

        // Count occurrences of EOF marker
        let eof_count = output.windows(BGZF_EOF.len()).filter(|w| *w == BGZF_EOF).count();
        assert_eq!(eof_count, 1, "EOF marker should appear exactly once");
    }

    /// Test that EOF marker is written even when the buffer is empty.
    #[test]
    fn test_eof_marker_empty_write() {
        let mut output = Vec::new();
        {
            let writer = Writer::new(&mut output, CompressionLevel::new(3).unwrap());
            // Don't write any data, just finish
            writer.finish().unwrap();
        }

        // Should still have the EOF marker
        assert!(
            output.ends_with(BGZF_EOF),
            "Output should end with BGZF_EOF marker even with no data written"
        );
        // With no data, output should be exactly the EOF marker
        assert_eq!(output.as_slice(), BGZF_EOF);
    }

    /// Test that calling flush() multiple times doesn't write multiple EOF markers.
    #[test]
    fn test_multiple_flush_single_eof() {
        let mut output = Vec::new();
        {
            let mut writer = Writer::new(&mut output, CompressionLevel::new(3).unwrap());
            writer.write_all(b"hello").unwrap();
            writer.flush().unwrap();
            writer.write_all(b"world").unwrap();
            writer.flush().unwrap();
            writer.finish().unwrap();
        }

        // Verify EOF marker appears exactly once at the end
        assert!(output.ends_with(BGZF_EOF), "Output should end with BGZF_EOF marker");

        // Count occurrences of EOF marker
        let eof_count = output.windows(BGZF_EOF.len()).filter(|w| *w == BGZF_EOF).count();
        assert_eq!(
            eof_count, 1,
            "EOF marker should appear exactly once even after multiple flush() calls"
        );
    }

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
        bgzf.finish().unwrap();

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

    /// A block whose footer ISIZE claims more uncompressed bytes than the DEFLATE
    /// payload actually produces must be rejected as corrupt — never read past the
    /// region the decompressor initialized (which the reader now leaves uninitialized).
    #[test]
    fn block_claiming_more_bytes_than_payload_is_rejected() {
        let mut compressor = Compressor::new(CompressionLevel::new(3).unwrap());
        let mut block = vec![];
        compressor.compress(b"hello world", &mut block).unwrap();

        // Inflate the footer's ISIZE (last four bytes, little-endian) past the real size.
        let len = block.len();
        block[len - 4..].copy_from_slice(&200u32.to_le_bytes());

        let mut out = vec![];
        let result = Reader::new(block.as_slice()).read_to_end(&mut out);
        assert!(result.is_err(), "block whose ISIZE exceeds its payload must error");
    }

    /// A compressed block whose footer ISIZE exceeds the maximum BGZF block size must be rejected
    /// with [`BgzfError::UncompressedSizeExceeded`] rather than panicking against the fixed-size
    /// decompression buffer. A compressed (non-stored) block is used so the reader takes the
    /// libdeflate path where that guard lives.
    #[test]
    fn block_with_oversized_isize_is_rejected() {
        let mut compressor = Compressor::new(CompressionLevel::new(6).unwrap());
        let mut block = vec![];
        compressor.compress(&[b'A'; 1024], &mut block).unwrap(); // compresses, so not a stored block

        // Claim far more uncompressed bytes than the buffer can hold (last four bytes = ISIZE).
        let len = block.len();
        block[len - 4..].copy_from_slice(&100_000u32.to_le_bytes());

        let mut out = vec![];
        let err = Reader::new(block.as_slice())
            .read_to_end(&mut out)
            .expect_err("ISIZE beyond the max block size must error, not panic");
        let bgzf = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<BgzfError>())
            .expect("reader errors wrap a BgzfError");
        assert!(
            matches!(bgzf, BgzfError::UncompressedSizeExceeded { .. }),
            "expected UncompressedSizeExceeded, got {bgzf:?}"
        );
    }

    /// Compression level 0 must emit a single final DEFLATE stored block — the shape the
    /// reader's fast path detects and copies without invoking libdeflate.
    #[test]
    fn level_0_emits_a_single_final_stored_block() {
        let mut compressor = Compressor::new(CompressionLevel::new(0).unwrap());
        let input = b"the quick brown fox jumps over the lazy dog";
        let mut block = vec![];
        compressor.compress(input, &mut block).unwrap();

        let deflate = &block[BGZF_HEADER_SIZE..block.len() - BGZF_FOOTER_SIZE];
        let len = stored_block_len(deflate).expect("level 0 should produce a stored block");
        assert_eq!(len, input.len());
        assert_eq!(&deflate[DEFLATE_STORED_HEADER_SIZE..], input);
    }

    /// Real compressed data (level 6) is not a stored block, so the fast path must decline it
    /// and leave decompression to libdeflate.
    #[test]
    fn compressed_block_is_not_detected_as_stored() {
        let mut compressor = Compressor::new(CompressionLevel::new(6).unwrap());
        let input = vec![b'A'; 4096]; // highly compressible => real deflate, not a stored block
        let mut block = vec![];
        compressor.compress(&input, &mut block).unwrap();

        let deflate = &block[BGZF_HEADER_SIZE..block.len() - BGZF_FOOTER_SIZE];
        assert!(stored_block_len(deflate).is_none());
    }

    /// The store-only writer must frame output as DEFLATE stored blocks, produce the same bytes
    /// regardless of how writes are chunked across block boundaries, and round-trip.
    #[test]
    fn store_only_writer_emits_stored_blocks_across_chunked_writes() {
        let input: Vec<u8> = (0..150_000u32).map(|i| i.wrapping_mul(2_654_435_761) as u8).collect();

        let one_shot = {
            let mut out = vec![];
            let mut writer = Writer::new(&mut out, CompressionLevel::new(0).unwrap());
            writer.write_all(&input).unwrap();
            writer.finish().unwrap();
            out
        };
        let chunked = {
            let mut out = vec![];
            let mut writer = Writer::new(&mut out, CompressionLevel::new(0).unwrap());
            for chunk in input.chunks(7) {
                writer.write_all(chunk).unwrap();
            }
            writer.finish().unwrap();
            out
        };

        // Block boundaries depend only on the block size, so the framed output must be identical.
        assert_eq!(one_shot, chunked, "chunked writes must produce identical framing");

        // The first block must be a DEFLATE stored block...
        let first_deflate =
            &one_shot[BGZF_HEADER_SIZE..BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE];
        assert!(stored_block_len(first_deflate).is_some(), "level 0 must emit stored blocks");

        // ...and the whole thing must round-trip.
        let mut decoded = vec![];
        Reader::new(one_shot.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }

    /// At level 0, writing no data must still produce exactly the EOF marker — never a spurious
    /// empty stored block (guarded by `store_data_len > 0` in the writer's flush).
    #[test]
    fn store_only_empty_input_writes_only_eof() {
        let mut out = vec![];
        Writer::new(&mut out, CompressionLevel::new(0).unwrap()).finish().unwrap();
        assert_eq!(out.as_slice(), BGZF_EOF);
    }

    /// Input that is an exact multiple of the block size must emit full blocks with no trailing
    /// empty block, and still round-trip.
    #[test]
    fn store_only_exact_block_multiple_has_no_trailing_empty_block() {
        let blocksize = 1024;
        let input = vec![0x5Au8; blocksize * 3];

        let mut out = vec![];
        let mut writer =
            Writer::with_capacity(&mut out, CompressionLevel::new(0).unwrap(), blocksize);
        writer.write_all(&input).unwrap();
        writer.finish().unwrap();

        // Exactly three full stored blocks followed by the EOF marker — nothing else.
        let block_bytes =
            BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE + blocksize + BGZF_FOOTER_SIZE;
        assert_eq!(out.len(), block_bytes * 3 + BGZF_EOF.len());

        let mut decoded = vec![];
        Reader::new(out.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }

    /// At level 0 the EOF marker must be written exactly once when relying on Drop.
    #[test]
    fn store_only_eof_written_once_on_drop() {
        let mut out = vec![];
        {
            let mut writer = Writer::new(&mut out, CompressionLevel::new(0).unwrap());
            writer.write_all(b"some store-only data").unwrap();
        }
        assert!(out.ends_with(BGZF_EOF), "output should end with the EOF marker");
        let eof_count = out.windows(BGZF_EOF.len()).filter(|w| *w == BGZF_EOF).count();
        assert_eq!(eof_count, 1, "EOF marker should appear exactly once");
    }

    /// The reader must round-trip multi-block store-only data, exercising the stored-block fast
    /// path across several blocks.
    #[test]
    fn reader_round_trips_store_only_data() {
        let input: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut blob = vec![];
        let mut writer = Writer::new(&mut blob, CompressionLevel::new(0).unwrap());
        writer.write_all(&input).unwrap();
        writer.finish().unwrap();

        let mut decoded = vec![];
        Reader::new(blob.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }

    const DICT_SIZE: usize = 32768;
    proptest! {
        #[test]
        fn proptest_bgzf(
            input in prop::collection::vec(0..u8::MAX, 1..(DICT_SIZE * 10)),
            buf_size in DICT_SIZE..BGZF_BLOCK_SIZE,
            write_size in 1..BGZF_BLOCK_SIZE * 4,
            comp_level in 0..=12_u8
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
            writer.finish().unwrap();

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
