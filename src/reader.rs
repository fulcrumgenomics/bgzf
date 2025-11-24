//! A Reader for BGZF compressed data.
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use bytes::{Buf, BytesMut};

use crate::{
    check_header, get_block_size, get_footer_values, strip_footer, Decompressor, BGZF_BLOCK_SIZE,
    BGZF_HEADER_SIZE, BUFSIZE,
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
    decompressed_buffer: BytesMut,
    compressed_buffer: BytesMut,
    header_buffer: Vec<u8>,
    decompressor: Decompressor,
    reader: R,
}

impl<R> Reader<R>
where
    R: Read,
{
    pub fn new(reader: R) -> Self {
        let decompressor = Decompressor::new();

        Self {
            decompressed_buffer: BytesMut::with_capacity(BUFSIZE),
            compressed_buffer: BytesMut::with_capacity(BGZF_BLOCK_SIZE),
            header_buffer: vec![0; BGZF_HEADER_SIZE],
            decompressor,
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
    /// - `Err(..)` means that an error has ocurred
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut total_bytes_copied = 0;
        loop {
            let available_bytes = self.decompressed_buffer.remaining();
            let remaining_bytes_needed = buf.len() - total_bytes_copied;
            // There are bytes we've already decompressed but haven't copied to the output buffer yet
            if available_bytes > remaining_bytes_needed {
                // The total decompressed is greater than the output buffer
                self.decompressed_buffer.copy_to_slice(&mut buf[total_bytes_copied..]);
            } else if !self.decompressed_buffer.is_empty() {
                // The total decompressed is less than the output buffer
                self.decompressed_buffer.copy_to_slice(
                    &mut buf[total_bytes_copied..total_bytes_copied + available_bytes],
                );
            }
            total_bytes_copied += available_bytes - self.decompressed_buffer.remaining();

            // Check if we've filled the output buffer. If it hasn't been filled then decompress another block.
            if total_bytes_copied == buf.len() {
                // The output buffer has been filled, return
                break;
            }

            debug_assert!(
                total_bytes_copied < buf.len(),
                "Check that we haven't somehow ended up with more bytes than should be possible."
            );

            // The output buffer hasn't been filled, try to decompress another block. If another
            // block is not available then we are done.
            self.header_buffer.fill(0);
            if let Ok(()) = self.reader.read_exact(&mut self.header_buffer) {
                check_header(&self.header_buffer)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let size = get_block_size(&self.header_buffer);

                self.compressed_buffer.clear();
                self.compressed_buffer.resize(size - BGZF_HEADER_SIZE, 0);
                self.reader.read_exact(&mut self.compressed_buffer)?;

                let check = get_footer_values(&self.compressed_buffer);
                self.decompressed_buffer.clear();
                self.decompressed_buffer.resize(check.amount as usize, 0);

                self.decompressor
                    .decompress(
                        strip_footer(&self.compressed_buffer),
                        &mut self.decompressed_buffer,
                        check,
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            } else {
                break;
            }
        }

        Ok(total_bytes_copied)
    }
}
