//! A BGZF writer implementation.
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use bytes::BytesMut;
use libdeflater::Crc;

use crate::{
    header_inner, CompressionLevel, Compressor, BGZF_BLOCK_SIZE, BGZF_EOF, BGZF_FOOTER_SIZE,
    BGZF_HEADER_SIZE, BGZF_SIZEOF_CRC32, BUFSIZE, DEFLATE_STORED_HEADER_SIZE, MAX_BGZF_BLOCK_SIZE,
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
    /// Buffer of not-yet-compressed bytes (compress path only).
    uncompressed_buffer: BytesMut,
    /// Reusable output buffer. On the compress path it holds one compressed block; on the
    /// store-only path it holds a fully framed DEFLATE stored block assembled in place.
    compressed_buffer: Vec<u8>,
    /// The size of the blocks to create
    blocksize: usize,
    /// The compression level, also recorded in each block header.
    level: CompressionLevel,
    /// The compressor to reuse (unused on the store-only path).
    compressor: Compressor,
    /// True at compression level 0: blocks are emitted as DEFLATE stored blocks without invoking
    /// libdeflate, accumulating straight into `compressed_buffer`.
    store_only: bool,
    /// Running CRC32 over the bytes accumulated in the current store-only block.
    store_crc: Crc,
    /// Number of data bytes accumulated in the current store-only block.
    store_data_len: usize,
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
        // Level 0 stores data uncompressed; assemble each framed block directly in this buffer.
        let store_only = u8::from(compression_level) == 0;
        let compressed_buffer = if store_only {
            vec![0u8; BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE + blocksize + BGZF_FOOTER_SIZE]
        } else {
            Vec::with_capacity(BUFSIZE)
        };
        Self {
            uncompressed_buffer: BytesMut::with_capacity(BUFSIZE),
            compressed_buffer,
            blocksize,
            level: compression_level,
            compressor,
            store_only,
            store_crc: Crc::new(),
            store_data_len: 0,
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
        if self.store_only {
            // Emit whatever partial block has accumulated; an empty block is never written.
            if self.store_data_len > 0 {
                self.emit_store_block()?;
            }
            return Ok(());
        }
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

    /// Accumulate `buf` into the current store-only block, emitting full blocks as they fill.
    fn write_store_only(&mut self, buf: &[u8]) -> io::Result<usize> {
        let data_offset = BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE;
        let mut remaining = buf;
        while !remaining.is_empty() {
            let n = (self.blocksize - self.store_data_len).min(remaining.len());
            let start = data_offset + self.store_data_len;
            self.compressed_buffer[start..start + n].copy_from_slice(&remaining[..n]);
            self.store_crc.update(&remaining[..n]);
            self.store_data_len += n;
            remaining = &remaining[n..];
            if self.store_data_len == self.blocksize {
                self.emit_store_block()?;
            }
        }
        Ok(buf.len())
    }

    /// Frame the data accumulated in `compressed_buffer` as a single DEFLATE stored block, write
    /// it to the inner writer, and reset for the next block.
    fn emit_store_block(&mut self) -> io::Result<()> {
        let data_len = self.store_data_len;
        let data_offset = BGZF_HEADER_SIZE + DEFLATE_STORED_HEADER_SIZE;

        // BGZF header. A stored block's "compressed" size is its 5-byte DEFLATE header plus data.
        let header = header_inner(self.level, (DEFLATE_STORED_HEADER_SIZE + data_len) as u16);
        self.compressed_buffer[..BGZF_HEADER_SIZE].copy_from_slice(&header);

        // DEFLATE stored-block header: BFINAL=1, BTYPE=00, then LEN and its complement NLEN.
        let len = data_len as u16;
        self.compressed_buffer[BGZF_HEADER_SIZE] = 0b001;
        self.compressed_buffer[BGZF_HEADER_SIZE + 1..BGZF_HEADER_SIZE + 3]
            .copy_from_slice(&len.to_le_bytes());
        self.compressed_buffer[BGZF_HEADER_SIZE + 3..BGZF_HEADER_SIZE + 5]
            .copy_from_slice(&(!len).to_le_bytes());

        // BGZF footer: CRC32 of the data followed by the uncompressed size.
        let footer_offset = data_offset + data_len;
        self.compressed_buffer[footer_offset..footer_offset + BGZF_SIZEOF_CRC32]
            .copy_from_slice(&self.store_crc.sum().to_le_bytes());
        self.compressed_buffer[footer_offset + BGZF_SIZEOF_CRC32..footer_offset + BGZF_FOOTER_SIZE]
            .copy_from_slice(&(data_len as u32).to_le_bytes());

        let end = footer_offset + BGZF_FOOTER_SIZE;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "writer already finished"))?;
        writer.write_all(&self.compressed_buffer[..end])?;

        self.store_crc = Crc::new();
        self.store_data_len = 0;
        Ok(())
    }
}

impl Writer<File> {
    /// Create a BGZF writer from a [`Path`].
    pub fn from_path<P>(path: P, compression_level: CompressionLevel) -> io::Result<Self>
    where
        P: AsRef<Path>,
    {
        File::create(path).map(|f| Self::new(f, compression_level))
    }
}

impl Writer<BufWriter<File>> {
    /// Create a buffered BGZF writer from a [`Path`].
    ///
    /// Uses a 256KiB buffer to batch write syscalls. This may improve
    /// performance when writing to files, especially on high-latency storage.
    pub fn from_path_buffered<P>(path: P, compression_level: CompressionLevel) -> io::Result<Self>
    where
        P: AsRef<Path>,
    {
        File::create(path)
            .map(|f| Self::new(BufWriter::with_capacity(256 * 1024, f), compression_level))
    }
}

impl<W> Write for Writer<W>
where
    W: Write,
{
    /// Write a buffer into this writer, returning how many bytes were written.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.store_only {
            if self.writer.is_none() {
                return Err(io::Error::new(io::ErrorKind::Other, "writer already finished"));
            }
            return self.write_store_only(buf);
        }
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
