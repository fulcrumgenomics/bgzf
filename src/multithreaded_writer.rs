//! A per-instance multithreaded BGZF writer.
//!
//! [`MultithreadedWriter`] compresses BGZF blocks on a pool of worker threads it owns,
//! while presenting a plain [`std::io::Write`] to the caller. It is a drop-in,
//! higher-throughput alternative to the single-threaded [`Writer`](crate::Writer) for large
//! streams; its output is a valid BGZF stream decodable by any BGZF reader.
//!
//! # Design
//!
//! The mirror image of [`MultithreadedReader`](crate::MultithreadedReader), modelled on
//! noodles-bgzf's `MultithreadedWriter`:
//!
//! - **Caller** (the `Write` impl) — accumulates bytes into a staging buffer and, each time a
//!   block fills, hands it to the worker pool while registering its slot with the writer
//!   thread so output order is preserved.
//! - **Deflater workers** — compress blocks in parallel using this crate's
//!   [`Compressor`](crate::Compressor) (so compression level 0 emits stored blocks, exactly
//!   like the single-threaded writer).
//! - **Writer thread** — pulls compressed blocks *in submission order*, writes them to the
//!   inner writer, and appends the BGZF EOF marker on shutdown.
//!
//! Block ordering falls out of a FIFO of per-block one-shot channels: the caller pushes each
//! block's result-receiver onto the writer thread's queue in order, and the writer thread
//! drains them in that order, blocking on each until its worker finishes. Workers may finish
//! out of order; the output never does.
//!
//! As with the reader, the channels are sized to `worker_count.max(MIN_BUFFERS)` so a
//! single-worker writer still has blocks in flight rather than stalling on a synchronous
//! per-block handoff.

use std::io::{self, Write};
use std::num::NonZero;
use std::thread::{self, JoinHandle};

use bytes::{Bytes, BytesMut};
use kanal::{bounded, Receiver, Sender};

use crate::{BgzfError, CompressionLevel, Compressor, BGZF_BLOCK_SIZE, BGZF_EOF};

/// Write-ahead depth floor; see [`MultithreadedReader`](crate::MultithreadedReader)'s
/// `MIN_BUFFERS` for the rationale (a single-worker writer must still pipeline).
const MIN_BUFFERS: usize = 8;

// A per-block "one-shot": a worker sends the framed, compressed block (or a compression
// error) back on it, and the writer thread — holding the matching receiver, pulled in
// submission order — waits on it.
type Compressed = io::Result<Vec<u8>>;
type CompressedTx = Sender<Compressed>;
type CompressedRx = Receiver<Compressed>;
// Caller → workers: the uncompressed block plus the one-shot to answer on.
type DeflateTx = Sender<(Bytes, CompressedTx)>;
type DeflateRx = Receiver<(Bytes, CompressedTx)>;
// Caller → writer thread: the one-shot receivers, in submission order.
type OrderTx = Sender<CompressedRx>;
type OrderRx = Receiver<CompressedRx>;

enum State<W> {
    Running {
        writer_handle: JoinHandle<io::Result<W>>,
        deflater_handles: Vec<JoinHandle<()>>,
        deflate_tx: DeflateTx,
        order_tx: OrderTx,
    },
    Done,
}

/// A multithreaded BGZF writer.
///
/// Compresses blocks on a dedicated pool of worker threads while writing them, in order, to
/// the inner writer. See the [module docs](self) for the design.
///
/// # Finishing
///
/// Like the single-threaded [`Writer`](crate::Writer), you should call
/// [`finish`](Self::finish) when done: it flushes buffered data, drains the worker pool,
/// writes the BGZF EOF marker, and surfaces any compression or I/O error. If the writer is
/// dropped instead, finishing still happens but errors are silently ignored.
///
/// # Example
///
/// ```rust
/// # #[cfg(feature = "multithreading-simple")] {
/// use std::io::Write;
/// use bgzf::{CompressionLevel, MultithreadedWriter};
/// let mut writer = MultithreadedWriter::new(vec![], CompressionLevel::new(6).unwrap());
/// writer.write_all(b"hello world").unwrap();
/// let compressed = writer.finish().unwrap();
/// # }
/// ```
pub struct MultithreadedWriter<W>
where
    W: Write + Send + 'static,
{
    state: State<W>,
    /// Staging buffer of not-yet-dispatched uncompressed bytes for the current block.
    buf: BytesMut,
    /// Uncompressed bytes per block.
    blocksize: usize,
}

impl<W> MultithreadedWriter<W>
where
    W: Write + Send + 'static,
{
    /// Create a writer with a worker count derived from the available parallelism.
    pub fn new(inner: W, compression_level: CompressionLevel) -> Self {
        let workers = thread::available_parallelism().map_or(1, NonZero::get);
        Self::with_worker_count(
            NonZero::new(workers).unwrap_or(NonZero::<usize>::MIN),
            inner,
            compression_level,
        )
    }

    /// Create a writer with the given number of deflater worker threads.
    pub fn with_worker_count(
        worker_count: NonZero<usize>,
        inner: W,
        compression_level: CompressionLevel,
    ) -> Self {
        Self::with_capacity(worker_count, inner, compression_level, BGZF_BLOCK_SIZE)
    }

    /// Create a writer with the given worker count and uncompressed block size.
    ///
    /// # Panics
    ///
    /// Panics if `blocksize` is not in `1..=BGZF_BLOCK_SIZE`.
    pub fn with_capacity(
        worker_count: NonZero<usize>,
        inner: W,
        compression_level: CompressionLevel,
        blocksize: usize,
    ) -> Self {
        assert!(
            (1..=BGZF_BLOCK_SIZE).contains(&blocksize),
            "blocksize must be in 1..={BGZF_BLOCK_SIZE}"
        );
        let capacity = worker_count.get().max(MIN_BUFFERS);

        let (deflate_tx, deflate_rx) = bounded::<(Bytes, CompressedTx)>(capacity);
        let (order_tx, order_rx) = bounded::<CompressedRx>(capacity);

        let deflater_handles = spawn_deflaters(compression_level, worker_count.get(), deflate_rx);
        let writer_handle = spawn_writer(inner, order_rx);

        Self {
            state: State::Running { writer_handle, deflater_handles, deflate_tx, order_tx },
            buf: BytesMut::with_capacity(blocksize),
            blocksize,
        }
    }

    /// Flush buffered data, drain the worker pool, write the BGZF EOF marker, and return the
    /// inner writer.
    ///
    /// Prefer this over dropping the writer: it surfaces compression and I/O errors that the
    /// `Drop` path would silently swallow, and guarantees the EOF marker is written exactly
    /// once.
    pub fn finish(&mut self) -> io::Result<W> {
        if !self.buf.is_empty() {
            self.send()?;
        }
        match std::mem::replace(&mut self.state, State::Done) {
            State::Running { writer_handle, mut deflater_handles, deflate_tx, order_tx } => {
                // Close the deflater input so workers finish once they have drained it, then
                // join them. Closing the order channel afterwards lets the writer thread
                // drain its queue, append EOF, and return the inner writer.
                drop(deflate_tx);
                for handle in deflater_handles.drain(..) {
                    handle.join().map_err(|_| thread_panicked("deflater"))?;
                }
                drop(order_tx);
                writer_handle.join().map_err(|_| thread_panicked("writer"))?
            }
            State::Done => Err(io::Error::new(io::ErrorKind::Other, "writer already finished")),
        }
    }

    /// Dispatch the staging buffer to the worker pool as one block, preserving order.
    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let State::Running { deflate_tx, order_tx, .. } = &self.state else {
            return Err(io::Error::new(io::ErrorKind::Other, "writer already finished"));
        };

        let data = self.buf.split().freeze();
        let (compressed_tx, compressed_rx) = bounded::<Compressed>(1);
        deflate_tx
            .send((data, compressed_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "bgzf writer pipeline stopped"))?;
        order_tx
            .send(compressed_rx)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "bgzf writer pipeline stopped"))?;
        Ok(())
    }
}

impl<W> Write for MultithreadedWriter<W>
where
    W: Write + Send + 'static,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = (self.blocksize - self.buf.len()).min(buf.len());
        self.buf.extend_from_slice(&buf[..n]);
        if self.buf.len() >= self.blocksize {
            self.send()?;
        }
        Ok(n)
    }

    /// Dispatch any buffered data as a block. Note this does not wait for that block to reach
    /// the inner writer, nor flush the inner writer (which the writer thread owns) — call
    /// [`finish`](Self::finish) to fully drain the pipeline.
    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

impl<W> Drop for MultithreadedWriter<W>
where
    W: Write + Send + 'static,
{
    fn drop(&mut self) {
        if matches!(self.state, State::Running { .. }) {
            let _ = self.finish();
        }
    }
}

impl MultithreadedWriter<std::fs::File> {
    /// Create a multithreaded writer over a new file at `path`.
    pub fn from_path<P: AsRef<std::path::Path>>(
        path: P,
        compression_level: CompressionLevel,
    ) -> io::Result<Self> {
        std::fs::File::create(path).map(|f| Self::new(f, compression_level))
    }
}

fn thread_panicked(which: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("bgzf {which} thread panicked"))
}

/// The deflater workers: compress each block and answer its one-shot with the framed bytes.
fn spawn_deflaters(
    compression_level: CompressionLevel,
    worker_count: usize,
    deflate_rx: DeflateRx,
) -> Vec<JoinHandle<()>> {
    (0..worker_count)
        .map(|_| {
            let deflate_rx = deflate_rx.clone();
            thread::spawn(move || {
                let mut compressor = Compressor::new(compression_level);
                while let Ok((data, compressed_tx)) = deflate_rx.recv() {
                    // v1 allocates one output buffer per block; recycling is a future,
                    // bench-gated optimization (see CHANGELOG).
                    let mut out = Vec::new();
                    let result = compressor.compress(&data, &mut out).map(|()| out).map_err(to_io);
                    compressed_tx.send(result).ok();
                }
            })
        })
        .collect()
}

/// The writer thread: write compressed blocks in submission order, then append EOF.
fn spawn_writer<W>(mut writer: W, order_rx: OrderRx) -> JoinHandle<io::Result<W>>
where
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        while let Ok(compressed_rx) = order_rx.recv() {
            let block = compressed_rx.recv().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "bgzf deflater thread stopped")
            })??;
            writer.write_all(&block)?;
        }
        writer.write_all(BGZF_EOF)?;
        writer.flush()?;
        Ok(writer)
    })
}

fn to_io(e: BgzfError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    use crate::{CompressionLevel, Reader};

    use super::*;

    fn level(n: u8) -> CompressionLevel {
        CompressionLevel::new(n).unwrap()
    }

    /// A deterministic, multi-block payload.
    fn sample(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| i.wrapping_mul(2_654_435_761).rotate_left(13) as u8).collect()
    }

    /// An owned, `'static + Send` sink whose bytes remain inspectable after the writer that
    /// held it is dropped — needed to test the `Drop` path (which discards the inner writer).
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(vec![])))
        }
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn decode(blob: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        Reader::new(blob).read_to_end(&mut out).unwrap();
        out
    }

    /// Compress `input` with the multithreaded writer, reclaiming the framed blob via `finish`.
    fn write_mt(input: &[u8], workers: usize, comp_level: u8) -> Vec<u8> {
        let mut writer = MultithreadedWriter::with_worker_count(
            NonZero::new(workers).unwrap(),
            Vec::new(),
            level(comp_level),
        );
        writer.write_all(input).unwrap();
        writer.finish().unwrap()
    }

    /// The multithreaded writer's output must round-trip through the single-threaded reader,
    /// at every worker count and compression level, for multi-block input.
    #[test]
    fn round_trips_through_serial_reader() {
        let input = sample(300_000);
        for comp_level in [0u8, 1, 6] {
            for workers in [1usize, 2, 4, 8] {
                assert_eq!(
                    decode(&write_mt(&input, workers, comp_level)),
                    input,
                    "mt writer diverged at level {comp_level}, {workers} workers"
                );
            }
        }
    }

    /// Output must be independent of how writes are chunked across block boundaries.
    #[test]
    fn output_independent_of_write_chunking() {
        let input = sample(200_000);

        let one_shot = write_mt(&input, 4, 6);

        let chunked = {
            let mut w = MultithreadedWriter::with_worker_count(
                NonZero::new(4).unwrap(),
                Vec::new(),
                level(6),
            );
            for chunk in input.chunks(7) {
                w.write_all(chunk).unwrap();
            }
            w.finish().unwrap()
        };

        // Block boundaries depend only on block size, so the framing must be identical
        // regardless of write chunking.
        assert_eq!(one_shot, chunked);
    }

    /// Writing nothing must still produce exactly the EOF marker.
    #[test]
    fn empty_writes_only_eof() {
        let out = MultithreadedWriter::new(Vec::new(), level(6)).finish().unwrap();
        assert_eq!(out.as_slice(), BGZF_EOF);
    }

    /// The EOF marker must appear exactly once, whether finishing explicitly or via drop.
    #[test]
    fn eof_written_once() {
        for use_finish in [true, false] {
            let sink = SharedBuf::new();
            {
                let mut w = MultithreadedWriter::with_worker_count(
                    NonZero::new(3).unwrap(),
                    sink.clone(),
                    level(6),
                );
                w.write_all(b"some data to compress").unwrap();
                if use_finish {
                    w.finish().unwrap();
                }
            }
            let out = sink.bytes();
            assert!(
                out.ends_with(BGZF_EOF),
                "output must end with the EOF marker (finish={use_finish})"
            );
            let eof_count = out.windows(BGZF_EOF.len()).filter(|w| *w == BGZF_EOF).count();
            assert_eq!(eof_count, 1, "EOF marker should appear exactly once (finish={use_finish})");
        }
    }

    /// Level 0 through the multithreaded writer must produce store-only output that the
    /// reader round-trips (exercises the stored-block path end to end).
    #[test]
    fn level_zero_round_trips() {
        let input = sample(250_000);
        assert_eq!(decode(&write_mt(&input, 4, 0)), input);
    }

    /// The multithreaded writer splits blocks at the same boundaries and compresses each with
    /// the same `Compressor` as the single-threaded [`Writer`](crate::Writer), so for the same
    /// input its framed output must be byte-for-byte identical — at every level including 0.
    #[test]
    fn matches_single_threaded_writer_byte_for_byte() {
        use crate::Writer;

        let input = sample(200_000);
        for comp_level in [0u8, 1, 6, 9] {
            let serial = {
                let mut out = vec![];
                let mut w = Writer::new(&mut out, level(comp_level));
                w.write_all(&input).unwrap();
                w.finish().unwrap();
                out
            };
            let mt = write_mt(&input, 4, comp_level);
            assert_eq!(mt, serial, "mt vs serial writer framing differs at level {comp_level}");
        }
    }

    /// End-to-end through both multithreaded halves.
    #[test]
    fn mt_writer_to_mt_reader_round_trips() {
        use std::io::Cursor;

        use crate::MultithreadedReader;

        let input = sample(400_000);
        for comp_level in [0u8, 6] {
            let compressed = write_mt(&input, 4, comp_level);
            let mut out = vec![];
            MultithreadedReader::with_worker_count(
                NonZero::new(3).unwrap(),
                Cursor::new(compressed),
            )
            .read_to_end(&mut out)
            .unwrap();
            assert_eq!(out, input, "mt->mt round trip failed at level {comp_level}");
        }
    }

    /// `flush()` mid-stream forces a block boundary but must not corrupt the stream.
    #[test]
    fn flush_midstream_round_trips() {
        let input = sample(100_000);
        let mut w =
            MultithreadedWriter::with_worker_count(NonZero::new(4).unwrap(), Vec::new(), level(6));
        w.write_all(&input[..40_000]).unwrap();
        w.flush().unwrap();
        w.write_all(&input[40_000..]).unwrap();
        let compressed = w.finish().unwrap();
        assert_eq!(decode(&compressed), input);
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// For arbitrary input, level, and worker count, the multithreaded writer's output must
        /// round-trip through the single-threaded reader.
        #[test]
        fn proptest_mt_writer_round_trips(
            input in prop::collection::vec(any::<u8>(), 1..100_000usize),
            comp_level in 0..=12u8,
            workers in 1usize..=4,
        ) {
            let blob = write_mt(&input, workers, comp_level);
            prop_assert_eq!(decode(&blob), input);
        }
    }
}
