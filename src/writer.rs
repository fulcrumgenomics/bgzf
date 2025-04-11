//! A BGZF writer implementation.
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

use bytes::BytesMut;

use crate::{
    BGZF_BLOCK_SIZE, BGZF_EOF, BUFSIZE, CompressionLevel, Compressor, MAX_BGZF_BLOCK_SIZE,
};

/// A BGZF writer.
///
/// # Example
///
/// ```rust
/// use bgzf::{CompressionLevel, Writer};
/// use std::error::Error;
/// use std::io::Write;
///
/// fn main() -> Result<(), Box<dyn Error>> {
///     // Write compressed data
///     let mut destination = vec![];
///     let mut writer = Writer::new(&mut destination, 2.try_into()?);
///     let input = &[b'A'; 100];
///     writer.write_all(input)?;
///     writer.flush()?;
///     drop(writer);
///
///     assert!(destination.len() < input.len());
///     Ok(())
/// }
/// ```
pub struct Writer<W>
where
    W: Write,
{
    /// The internal buffer to use
    uncompressed_buffer: BytesMut,
    /// The buffer to reuse for compressed bytes
    compressed_buffer: Vec<u8>,
    /// The size of the blocks to create
    blocksize: usize,
    /// The compressor to reuse
    compressor: Compressor,
    /// The inner writer
    writer: W,
}

impl<W> Writer<W>
where
    W: Write,
{
    /// Create a new [`Writer`]
    pub fn new(writer: W, compression_level: CompressionLevel) -> Self {
        Self::with_capacity(writer, compression_level, BGZF_BLOCK_SIZE)
    }

    /// Create a writer with a set capacity.
    ///
    /// By default the capacity is [`bgzf::BUFSIZE`]. The capacity bust be less than [`bgzf::BGZF_BLOCK_SIZE`].
    pub fn with_capacity(writer: W, compression_level: CompressionLevel, blocksize: usize) -> Self {
        assert!(blocksize <= BGZF_BLOCK_SIZE);
        let compressor = Compressor::new(compression_level);
        Self {
            uncompressed_buffer: BytesMut::with_capacity(BUFSIZE),
            compressed_buffer: Vec::with_capacity(BUFSIZE),
            blocksize,
            compressor,
            writer,
        }
    }
}

impl Writer<File> {
    /// Create a BGZF writer from a [`Path`].
    pub fn from_path<P>(path: P, compression_level: CompressionLevel) -> io::Result<Self>
    where
        P: AsRef<Path>,
    {
        // TODO: benchmark whether there is any benefit to using a BufWriter
        File::create(path).map(|f| Self::new(f, compression_level))
    }
}

impl<W> Write for Writer<W>
where
    W: Write,
{
    /// Write a buffer into this writer, returning how many bytes were written.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.uncompressed_buffer.extend_from_slice(buf);
        while self.uncompressed_buffer.len() >= self.blocksize {
            let b = self.uncompressed_buffer.split_to(self.blocksize).freeze();
            self.compressor
                .compress(&b[..], &mut self.compressed_buffer)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.writer.write_all(&self.compressed_buffer)?;
            self.compressed_buffer.clear();
        }
        Ok(buf.len())
    }

    /// Flush this output stream, ensuring all intermediately buffered contents are sent.
    fn flush(&mut self) -> std::io::Result<()> {
        while !self.uncompressed_buffer.is_empty() {
            let b = self
                .uncompressed_buffer
                .split_to(std::cmp::min(self.uncompressed_buffer.len(), MAX_BGZF_BLOCK_SIZE))
                .freeze();
            self.compressor
                .compress(&b[..], &mut self.compressed_buffer)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.writer.write_all(&self.compressed_buffer)?;
            self.compressed_buffer.clear();
            self.writer.write_all(BGZF_EOF)?; // this is an empty block
        }
        self.writer.flush()
    }
}

impl<W> Drop for Writer<W>
where
    W: Write,
{
    fn drop(&mut self) {
        self.flush().unwrap();
    }
}
