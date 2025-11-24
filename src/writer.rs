//! A BGZF writer implementation.
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

use bytes::BytesMut;

use crate::{
    CompressionLevel, Compressor, BGZF_BLOCK_SIZE, BGZF_EOF, BUFSIZE, MAX_BGZF_BLOCK_SIZE,
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
///     writer.finish()?;
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
    /// The inner writer, wrapped in Option to allow taking ownership in finish()
    writer: Option<W>,
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
    /// By default the capacity is [`bgzf::BUFSIZE`]. The capacity must be less than or equal to [`bgzf::BGZF_BLOCK_SIZE`].
    pub fn with_capacity(writer: W, compression_level: CompressionLevel, blocksize: usize) -> Self {
        assert!(blocksize <= BGZF_BLOCK_SIZE);
        let compressor = Compressor::new(compression_level);
        Self {
            uncompressed_buffer: BytesMut::with_capacity(BUFSIZE),
            compressed_buffer: Vec::with_capacity(BUFSIZE),
            blocksize,
            compressor,
            writer: Some(writer),
        }
    }

    /// Finish writing, flush all buffered data, write the BGZF EOF marker,
    /// and return the underlying writer.
    ///
    /// This method should be called when you are done writing to ensure the
    /// EOF marker is written exactly once. If this method is not called,
    /// the EOF marker will be written when the writer is dropped, but any
    /// errors will be silently ignored.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_buffer()?;
        let mut writer = self.writer.take().expect("writer already taken");
        writer.write_all(BGZF_EOF)?;
        writer.flush()?;
        Ok(writer)
    }

    /// Internal method to flush the uncompressed buffer without writing EOF.
    fn flush_buffer(&mut self) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "writer already finished"))?;
        while !self.uncompressed_buffer.is_empty() {
            let b = self
                .uncompressed_buffer
                .split_to(std::cmp::min(self.uncompressed_buffer.len(), MAX_BGZF_BLOCK_SIZE))
                .freeze();
            self.compressor
                .compress(&b[..], &mut self.compressed_buffer)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            writer.write_all(&self.compressed_buffer)?;
            self.compressed_buffer.clear();
        }
        Ok(())
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
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "writer already finished"))?;
        self.uncompressed_buffer.extend_from_slice(buf);
        while self.uncompressed_buffer.len() >= self.blocksize {
            let b = self.uncompressed_buffer.split_to(self.blocksize).freeze();
            self.compressor
                .compress(&b[..], &mut self.compressed_buffer)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            writer.write_all(&self.compressed_buffer)?;
            self.compressed_buffer.clear();
        }
        Ok(buf.len())
    }

    /// Flush this output stream, ensuring all intermediately buffered contents are sent.
    ///
    /// Note: This does NOT write the BGZF EOF marker. Call [`Writer::finish`] when
    /// you are done writing to properly finalize the BGZF file.
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_buffer()?;
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
}

impl<W> Drop for Writer<W>
where
    W: Write,
{
    fn drop(&mut self) {
        // Only write EOF if finish() wasn't called (writer is still Some)
        if self.writer.is_some() {
            // Flush buffer first (this borrows self mutably)
            let _ = self.flush_buffer();
            // Now we can borrow writer
            if let Some(ref mut writer) = self.writer {
                let _ = writer.write_all(BGZF_EOF);
                let _ = writer.flush();
            }
        }
    }
}
