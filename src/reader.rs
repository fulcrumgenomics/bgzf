//! A Reader for BGZF compressed data.
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use crate::{
    check_header, crc32, get_block_size, get_footer_values, stored_block_len, strip_footer,
    BgzfError, Decompressor, BGZF_FOOTER_SIZE, BGZF_HEADER_SIZE, DEFLATE_STORED_HEADER_SIZE,
    MAX_BGZF_BLOCK_SIZE,
};

/// A BGZF reader.
///
/// # Example
///
/// ```rust
/// use bgzf::{Reader, Compressor, CompressionLevel};
/// use std::error::Error;
/// use std::io::Read;
///
/// fn main() -> Result<(), Box<dyn Error>> {
///     // Create compressed data
///     let mut compressor = Compressor::new(CompressionLevel::new(2)?);
///     let input = &[b'A'; 100];
///     let mut compressed_data = vec![];
///     compressor.compress(input, &mut compressed_data)?;
///
///     let mut reader = Reader::new(compressed_data.as_slice());
///     let mut decompressed_data = vec![];
///     let _bytes_read = reader.read_to_end(&mut decompressed_data)?;
///     assert_eq!(decompressed_data, input);
///     Ok(())
/// }
/// ```
pub struct Reader<R>
where
    R: Read,
{
    /// Holds the current block's uncompressed bytes; callers are served out of this buffer. Stored
    /// blocks are read straight into it, inflated blocks are written into it by the decompressor.
    decompressed_buffer: Vec<u8>,
    /// Scratch buffer used only on the inflate path to assemble a block's raw DEFLATE payload.
    compressed_buffer: Vec<u8>,
    /// Start of the current block's unread uncompressed bytes within `decompressed_buffer`.
    block_pos: usize,
    /// End (exclusive) of the current block's uncompressed bytes within `decompressed_buffer`.
    block_end: usize,
    /// Holds the 18-byte BGZF header plus the following 5-byte DEFLATE block header, read together
    /// so the stored fast path can be chosen without a second read.
    header_buffer: Vec<u8>,
    decompressor: Decompressor,
    reader: R,
}

impl<R> Reader<R>
where
    R: Read,
{
    pub fn new(reader: R) -> Self {
        Self {
            // Pre-allocate to the maximum block sizes so no per-block resize is ever needed.
            decompressed_buffer: vec![0u8; MAX_BGZF_BLOCK_SIZE],
            compressed_buffer: vec![0u8; MAX_BGZF_BLOCK_SIZE],
            block_pos: 0,
            block_end: 0,
            header_buffer: vec![0u8; BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE],
            decompressor: Decompressor::new(),
            reader,
        }
    }
}

impl Reader<File> {
    /// Create a BGZF reader from a [`Path`].
    pub fn from_path<P>(path: P) -> io::Result<Self>
    where
        P: AsRef<Path>,
    {
        // TODO: benchmark whether there is any benefit to using a BufReader
        File::open(path).map(Self::new)
    }
}

impl<R> Read for Reader<R>
where
    R: Read,
{
    /// Attempt to read `buf.len()` bytes from source into `buf`.
    ///
    /// - `Ok(0)` means that EOF has been reached or `buf.len() == 0`.
    /// - `Ok(n < buf.len()` means that EOF has been reached.
    /// - `Err(..)` means that an error has occurred.
    ///
    /// A stream that ends cleanly on a block boundary is treated as end-of-input (a missing EOF
    /// marker is tolerated). A stream that ends partway through a block — a truncated block — is
    /// reported as an error rather than silently ignored.
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut total_bytes_copied = 0;
        loop {
            // Drain the current block before fetching the next one.
            let available = self.block_end - self.block_pos;
            if available > 0 {
                let n = available.min(buf.len() - total_bytes_copied);
                buf[total_bytes_copied..total_bytes_copied + n]
                    .copy_from_slice(&self.decompressed_buffer[self.block_pos..self.block_pos + n]);
                self.block_pos += n;
                total_bytes_copied += n;
            }

            if total_bytes_copied == buf.len() {
                break;
            }

            debug_assert!(total_bytes_copied < buf.len(), "More bytes copied than requested.");

            // Read the 18-byte BGZF header together with the 5-byte DEFLATE block header. Reading
            // both at once lets us choose the stored fast path without a second read: a clean EOF
            // at a block boundary stops the stream, while a partial read signals a truncated block.
            if !read_full(&mut self.reader, &mut self.header_buffer)? {
                break; // clean EOF
            }
            check_header(&self.header_buffer[..BGZF_HEADER_SIZE])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // A valid block always holds at least a header and footer; anything smaller is a
            // corrupt header and would underflow `payload_len`.
            let block_size = get_block_size(&self.header_buffer[..BGZF_HEADER_SIZE]);
            if block_size < BGZF_HEADER_SIZE + BGZF_FOOTER_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    BgzfError::InvalidHeader("block size smaller than header plus footer"),
                ));
            }
            let payload_len = block_size - BGZF_HEADER_SIZE;

            // The 5 bytes after the BGZF header are the DEFLATE block header. Copy them out so we
            // can both inspect them and, on the inflate path, prepend them to the payload.
            let deflate_header: [u8; DEFLATE_STORED_HEADER_SIZE] = self.header_buffer
                [BGZF_HEADER_SIZE..]
                .try_into()
                .expect("header_buffer holds the DEFLATE block header");

            // Fast path: a single final stored block that spans the whole payload holds its bytes
            // verbatim. Read the data and its footer straight into the decompressed buffer (at
            // offset 0, so callers drain an aligned, contiguous slice), skipping libdeflate and any
            // staging copy, then verify the uncompressed size and CRC.
            if let Some(len) = stored_block_len(&deflate_header) {
                let with_footer = len + BGZF_FOOTER_SIZE;
                if payload_len == DEFLATE_STORED_HEADER_SIZE + with_footer
                    && with_footer <= self.decompressed_buffer.len()
                {
                    self.reader.read_exact(&mut self.decompressed_buffer[..with_footer])?;
                    let check = get_footer_values(&self.decompressed_buffer[..with_footer]);
                    if check.amount as usize != len {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            BgzfError::InvalidHeader("stored block length disagrees with footer"),
                        ));
                    }
                    let found = crc32(&self.decompressed_buffer[..len]);
                    if found != check.sum {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            BgzfError::InvalidChecksum { found, expected: check.sum },
                        ));
                    }
                    self.block_pos = 0;
                    self.block_end = len;
                    continue;
                }
            }

            // Inflate path: reassemble the payload (the 5 header bytes already read plus the
            // remainder) into `compressed_buffer` and decompress it with libdeflate.
            self.compressed_buffer[..DEFLATE_STORED_HEADER_SIZE].copy_from_slice(&deflate_header);
            self.reader
                .read_exact(&mut self.compressed_buffer[DEFLATE_STORED_HEADER_SIZE..payload_len])?;

            let compressed = &self.compressed_buffer[..payload_len];
            let check = get_footer_values(compressed);
            let decompressed_len = check.amount as usize;

            // The decompressed buffer is sized to the BGZF maximum; a block claiming more is
            // corrupt and would otherwise index out of bounds.
            if decompressed_len > self.decompressed_buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    BgzfError::UncompressedSizeExceeded {
                        found: decompressed_len,
                        max: self.decompressed_buffer.len(),
                    },
                ));
            }

            self.decompressor
                .decompress(
                    strip_footer(compressed),
                    &mut self.decompressed_buffer[..decompressed_len],
                    check,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.block_pos = 0;
            self.block_end = decompressed_len;
        }

        Ok(total_bytes_copied)
    }
}

/// Fill `buf` completely from `reader`.
///
/// Returns `Ok(true)` once `buf` is full, `Ok(false)` if the stream ends cleanly before any byte is
/// read (a normal end-of-stream at a block boundary), and an error if the stream ends partway
/// through (a truncated block) or the underlying read fails.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated BGZF block"))
            }
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
