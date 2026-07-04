//! A per-instance multithreaded BGZF reader.
//!
//! [`MultithreadedReader`] decompresses BGZF blocks on a pool of worker threads it
//! owns, while presenting a plain [`std::io::Read`] (and [`std::io::BufRead`]) to the
//! caller. It is a drop-in, higher-throughput alternative to the single-threaded
//! [`Reader`](crate::Reader) for large, sequential streams; its decompressed output is
//! byte-identical to `Reader`'s at every worker count.
//!
//! # Design
//!
//! Three roles connected by [`kanal`] channels, modelled on noodles-bgzf's
//! `MultithreadedReader` but using this crate's block internals:
//!
//! - **Reader thread** — reads raw blocks (header + payload + footer) in file order,
//!   pulling a recycled buffer from a pool, and dispatches each to the worker pool while
//!   registering its slot with the consumer so order is preserved.
//! - **Inflater workers** — decompress blocks in parallel using the same logic as the
//!   single-threaded reader (stored-block fast path, else libdeflate; CRC32-verified).
//! - **Consumer** (the `Read` impl) — pulls decompressed blocks *in file order*, serves
//!   their bytes, and returns spent buffers to the pool.
//!
//! ## Read-ahead depth is decoupled from worker count
//!
//! All channels *and* the recycled-buffer pool are sized to
//! `worker_count.max(MIN_BUFFERS)`, never to `worker_count` alone. Sizing to the worker
//! count would leave a single-worker reader with depth-1 lookahead — the reader thread
//! and the inflater could not overlap with the consumer, and every block would be a
//! synchronous handoff. That makes a 1-worker multithreaded reader *slower* than the
//! single-threaded [`Reader`](crate::Reader); the `MIN_BUFFERS` floor is what prevents it.
//!
//! Seeking is intentionally out of scope: this reader is sequential only. A
//! `Seek`/virtual-position implementation could be layered on later for indexed reads.

use std::io::{self, BufRead, Read};
use std::num::NonZero;
use std::thread::{self, JoinHandle};

use kanal::{bounded, Receiver, Sender};

use crate::reader::read_full;
use crate::{
    check_header, crc32, get_block_size, get_footer_values, stored_block_len, strip_footer,
    BgzfError, Decompressor, BGZF_FOOTER_SIZE, BGZF_HEADER_SIZE, DEFLATE_STORED_HEADER_SIZE,
    MAX_BGZF_BLOCK_SIZE,
};

/// Read-ahead depth floor. See the module docs: channels and the buffer pool are sized to
/// `worker_count.max(MIN_BUFFERS)` so even a single-worker reader reads ahead.
const MIN_BUFFERS: usize = 8;

/// A recycled unit of work that cycles reader → worker → consumer → reader with no
/// per-block allocation.
#[derive(Default)]
struct Buffer {
    /// Raw block bytes: the 18-byte BGZF header + payload (deflate stream + 8-byte footer).
    raw: Vec<u8>,
    /// Decompressed block contents.
    data: Vec<u8>,
    /// Consumer read cursor into `data`.
    pos: usize,
}

// A per-block "one-shot": the worker sends the decoded (or failed) buffer back on it, and
// the consumer — holding the matching receiver, pulled in file order — waits on it.
type Decoded = io::Result<Buffer>;
type DecodedTx = Sender<Decoded>;
type DecodedRx = Receiver<Decoded>;
// Reader → workers: a raw block plus the one-shot to answer on.
type InflateTx = Sender<(Buffer, DecodedTx)>;
type InflateRx = Receiver<(Buffer, DecodedTx)>;
// Reader → consumer: the one-shot receivers, in file order.
type OrderTx = Sender<DecodedRx>;
type OrderRx = Receiver<DecodedRx>;
// Consumer → reader: spent buffers to reuse.
type RecycleTx = Sender<Buffer>;
type RecycleRx = Receiver<Buffer>;

enum State<R> {
    Running {
        reader_handle: JoinHandle<R>,
        inflater_handles: Vec<JoinHandle<()>>,
        order_rx: OrderRx,
        recycle_tx: RecycleTx,
    },
    Done,
}

/// A multithreaded BGZF reader.
///
/// Decompresses blocks on a dedicated pool of worker threads while exposing a sequential
/// [`Read`]/[`BufRead`] interface. See the [module docs](self) for the design.
///
/// The worker threads are joined when the reader is dropped; call [`finish`](Self::finish)
/// instead if you need to observe reader-thread panics or reclaim the inner reader.
///
/// # Example
///
/// ```rust
/// # #[cfg(feature = "multithreading-simple")] {
/// use std::io::Read;
/// use bgzf::MultithreadedReader;
/// # let compressed: &[u8] = &[];
/// let mut reader = MultithreadedReader::new(compressed);
/// let mut bytes = vec![];
/// reader.read_to_end(&mut bytes).unwrap();
/// # }
/// ```
pub struct MultithreadedReader<R>
where
    R: Read + Send + 'static,
{
    state: State<R>,
    /// The block currently being served to the caller.
    buffer: Buffer,
}

impl<R> MultithreadedReader<R>
where
    R: Read + Send + 'static,
{
    /// Create a reader with a worker count derived from the available parallelism.
    pub fn new(inner: R) -> Self {
        let workers = thread::available_parallelism().map_or(1, NonZero::get);
        Self::with_worker_count(NonZero::new(workers).unwrap_or(NonZero::<usize>::MIN), inner)
    }

    /// Create a reader with the given number of inflater worker threads.
    ///
    /// The read-ahead depth is `worker_count.max(8)` regardless of `worker_count`, so a
    /// single-worker reader still reads ahead (see the [module docs](self)).
    pub fn with_worker_count(worker_count: NonZero<usize>, inner: R) -> Self {
        let capacity = worker_count.get().max(MIN_BUFFERS);

        let (inflate_tx, inflate_rx) = bounded::<(Buffer, DecodedTx)>(capacity);
        let (order_tx, order_rx) = bounded::<DecodedRx>(capacity);
        let (recycle_tx, recycle_rx) = bounded::<Buffer>(capacity);

        // Seed the pool. `capacity` buffers cycle through the pipeline; together with the
        // one the consumer holds, exactly `capacity + 1` buffers ever exist, so the
        // `bounded(capacity)` recycle channel never overflows (the consumer always holds
        // one out). The reader thread always ends up blocked on `recycle_rx` when the
        // consumer stalls — which is why dropping `recycle_tx` is sufficient for shutdown.
        for _ in 0..capacity {
            recycle_tx.send(Buffer::default()).expect("seeding the recycle pool cannot fail");
        }

        let reader_handle = spawn_reader(inner, inflate_tx, order_tx, recycle_rx);
        let inflater_handles = spawn_inflaters(worker_count.get(), inflate_rx);

        Self {
            state: State::Running { reader_handle, inflater_handles, order_rx, recycle_tx },
            buffer: Buffer::default(),
        }
    }

    /// Shut the reader down and return the inner reader.
    ///
    /// Joins the worker threads and the reader thread. Unlike letting the reader drop, this
    /// surfaces a panic in any worker as an error. Errors from decoding blocks are reported
    /// through [`read`](Read::read) as they are encountered, not here.
    pub fn finish(&mut self) -> io::Result<R> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Running { reader_handle, mut inflater_handles, order_rx, recycle_tx } => {
                // Signal shutdown: with no more recycled buffers coming and no consumer for
                // the order channel, the reader thread breaks out of its loop, drops its
                // channel ends, and returns; the workers then see their input close.
                drop(recycle_tx);
                drop(order_rx);

                for handle in inflater_handles.drain(..) {
                    handle.join().map_err(|_| thread_panicked("inflater"))?;
                }
                reader_handle.join().map_err(|_| thread_panicked("reader"))
            }
            State::Done => Err(io::Error::new(io::ErrorKind::Other, "reader already finished")),
        }
    }

    /// Advance [`self.buffer`](Self::buffer) to the next non-empty decompressed block.
    ///
    /// Returns `Ok(true)` when a block is ready, `Ok(false)` at end of stream. Empty blocks
    /// (e.g. the BGZF EOF marker) are transparently skipped. Decode errors — and read or
    /// header errors surfaced by the reader thread — are returned here, in file order.
    fn next_block(&mut self) -> io::Result<bool> {
        let State::Running { order_rx, recycle_tx, .. } = &self.state else {
            return Ok(false);
        };
        loop {
            // Next block's one-shot, in file order; a closed channel means end of stream.
            let Ok(decoded_rx) = order_rx.recv() else {
                return Ok(false);
            };
            let buffer = decoded_rx.recv().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "bgzf worker thread stopped")
            })??;

            // Swap the new block in and recycle the one we just finished serving.
            let mut spent = std::mem::replace(&mut self.buffer, buffer);
            self.buffer.pos = 0;
            spent.raw.clear();
            spent.data.clear();
            spent.pos = 0;
            recycle_tx.send(spent).ok();

            if !self.buffer.data.is_empty() {
                return Ok(true);
            }
        }
    }
}

impl<R> Read for MultithreadedReader<R>
where
    R: Read + Send + 'static,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut copied = 0;
        while copied < buf.len() {
            if self.buffer.pos >= self.buffer.data.len() && !self.next_block()? {
                break;
            }
            let available = &self.buffer.data[self.buffer.pos..];
            let n = available.len().min(buf.len() - copied);
            buf[copied..copied + n].copy_from_slice(&available[..n]);
            self.buffer.pos += n;
            copied += n;
        }
        Ok(copied)
    }
}

impl<R> BufRead for MultithreadedReader<R>
where
    R: Read + Send + 'static,
{
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.buffer.pos >= self.buffer.data.len() {
            self.next_block()?;
        }
        Ok(&self.buffer.data[self.buffer.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.buffer.pos = (self.buffer.pos + amt).min(self.buffer.data.len());
    }
}

impl<R> Drop for MultithreadedReader<R>
where
    R: Read + Send + 'static,
{
    fn drop(&mut self) {
        if matches!(self.state, State::Running { .. }) {
            let _ = self.finish();
        }
    }
}

impl MultithreadedReader<std::fs::File> {
    /// Create a multithreaded reader over a file at `path`.
    pub fn from_path<P: AsRef<std::path::Path>>(path: P) -> io::Result<Self> {
        std::fs::File::open(path).map(Self::new)
    }
}

fn thread_panicked(which: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("bgzf {which} thread panicked"))
}

fn to_io(e: BgzfError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

/// The reader thread: read raw blocks in file order and dispatch them to the worker pool.
///
/// Read/format errors are pushed down the ordered one-shot pipeline so the consumer sees
/// them at the right point (matching the single-threaded reader), then the thread stops.
/// A clean end-of-stream at a block boundary simply ends the loop.
fn spawn_reader<R>(
    mut reader: R,
    inflate_tx: InflateTx,
    order_tx: OrderTx,
    recycle_rx: RecycleRx,
) -> JoinHandle<R>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        while let Ok(mut buffer) = recycle_rx.recv() {
            match read_raw_block(&mut reader, &mut buffer.raw) {
                Ok(false) => break, // clean end of stream at a block boundary
                Ok(true) => {
                    let (decoded_tx, decoded_rx) = bounded::<Decoded>(1);
                    // Dispatch to a worker, then register the slot with the consumer. If
                    // either channel is closed we are shutting down, so stop.
                    if inflate_tx.send((buffer, decoded_tx)).is_err()
                        || order_tx.send(decoded_rx).is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    // Surface the error to the consumer in order, then stop.
                    let (decoded_tx, decoded_rx) = bounded::<Decoded>(1);
                    decoded_tx.send(Err(e)).ok();
                    order_tx.send(decoded_rx).ok();
                    break;
                }
            }
        }
        reader
    })
}

/// The inflater workers: decode raw blocks into their buffers and answer their one-shots.
fn spawn_inflaters(worker_count: usize, inflate_rx: InflateRx) -> Vec<JoinHandle<()>> {
    (0..worker_count)
        .map(|_| {
            let inflate_rx = inflate_rx.clone();
            thread::spawn(move || {
                let mut decompressor = Decompressor::new();
                while let Ok((mut buffer, decoded_tx)) = inflate_rx.recv() {
                    let result = decode_block(&buffer.raw, &mut decompressor, &mut buffer.data)
                        .map(|()| buffer);
                    decoded_tx.send(result).ok();
                }
            })
        })
        .collect()
}

/// Read one raw BGZF block (header + payload + footer) into `raw`.
///
/// Returns `Ok(true)` when a block was read, `Ok(false)` on a clean end of stream at a block
/// boundary, and an error for a truncated or malformed block. Only the header is validated
/// here (matching the single-threaded reader); the footer and payload are validated by the
/// worker that decodes the block.
fn read_raw_block<R: Read>(reader: &mut R, raw: &mut Vec<u8>) -> io::Result<bool> {
    raw.resize(BGZF_HEADER_SIZE, 0);
    if !read_full(reader, &mut raw[..BGZF_HEADER_SIZE])? {
        return Ok(false);
    }
    check_header(&raw[..BGZF_HEADER_SIZE]).map_err(to_io)?;

    let block_size = get_block_size(&raw[..BGZF_HEADER_SIZE]);
    if block_size < BGZF_HEADER_SIZE + BGZF_FOOTER_SIZE {
        return Err(to_io(BgzfError::InvalidHeader("block size smaller than header plus footer")));
    }

    raw.resize(block_size, 0);
    reader.read_exact(&mut raw[BGZF_HEADER_SIZE..])?;
    Ok(true)
}

/// Decode one raw BGZF block into `out`, replacing its contents.
///
/// Mirrors the single-threaded reader's per-block path: a single final stored block is
/// copied verbatim (skipping libdeflate), anything else is inflated. In both cases the
/// uncompressed size is checked against the footer's ISIZE and the CRC32 is verified.
fn decode_block(raw: &[u8], decompressor: &mut Decompressor, out: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    // Everything after the 18-byte header: the deflate stream followed by the 8-byte footer.
    // `read_raw_block` guarantees `raw.len() >= BGZF_HEADER_SIZE + BGZF_FOOTER_SIZE`.
    let payload = &raw[BGZF_HEADER_SIZE..];
    let check = get_footer_values(payload);
    let expected_len = check.amount as usize;

    // Fast path: a single final stored block whose framing spans the whole payload holds its
    // bytes verbatim. Copy them out, skipping libdeflate, then verify size and CRC.
    if let Some(len) = stored_block_len(payload) {
        if payload.len() == DEFLATE_STORED_HEADER_SIZE + len + BGZF_FOOTER_SIZE {
            if len != expected_len {
                return Err(to_io(BgzfError::InvalidHeader(
                    "stored block length disagrees with footer",
                )));
            }
            let data = &payload[DEFLATE_STORED_HEADER_SIZE..DEFLATE_STORED_HEADER_SIZE + len];
            let found = crc32(data);
            if found != check.sum {
                return Err(to_io(BgzfError::InvalidChecksum { found, expected: check.sum }));
            }
            out.extend_from_slice(data);
            return Ok(());
        }
    }

    // Inflate path. Guard the claimed size against the maximum block size before allocating.
    if expected_len > MAX_BGZF_BLOCK_SIZE {
        return Err(to_io(BgzfError::UncompressedSizeExceeded {
            found: expected_len,
            max: MAX_BGZF_BLOCK_SIZE,
        }));
    }
    out.resize(expected_len, 0);
    decompressor.decompress(strip_footer(payload), out, check, true).map_err(to_io)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use crate::{CompressionLevel, Reader, Writer};

    use super::*;

    /// An owned, `'static + Send` reader over `blob` — what `MultithreadedReader` requires.
    fn owned(blob: &[u8]) -> Cursor<Vec<u8>> {
        Cursor::new(blob.to_vec())
    }

    /// BGZF-compress `data` at `level` into an in-memory blob.
    fn make_bgzf(data: &[u8], level: u8) -> Vec<u8> {
        let mut out = vec![];
        let mut writer = Writer::new(&mut out, CompressionLevel::new(level).unwrap());
        writer.write_all(data).unwrap();
        writer.finish().unwrap();
        out
    }

    /// A deterministic, multi-block payload (spans well over 64 KiB).
    fn sample(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| i.wrapping_mul(2_654_435_761).rotate_left(13) as u8).collect()
    }

    fn read_serial(blob: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        Reader::new(blob).read_to_end(&mut out).unwrap();
        out
    }

    fn read_mt(blob: &[u8], workers: usize) -> Vec<u8> {
        let mut out = vec![];
        MultithreadedReader::with_worker_count(NonZero::new(workers).unwrap(), owned(blob))
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    /// The multithreaded reader must produce byte-identical output to the single-threaded
    /// reader for the same input, at every worker count.
    #[test]
    fn matches_single_threaded_at_each_worker_count() {
        let input = sample(300_000); // several blocks
        for level in [0u8, 1, 6] {
            let blob = make_bgzf(&input, level);
            let serial = read_serial(&blob);
            assert_eq!(serial, input, "sanity: serial reader round-trips at level {level}");
            for workers in [1usize, 2, 4, 8] {
                assert_eq!(
                    read_mt(&blob, workers),
                    input,
                    "mt reader diverged at level {level}, {workers} workers"
                );
            }
        }
    }

    /// An explicit single-worker test: the depth-decoupling design must still be correct
    /// (not just fast) with one worker.
    #[test]
    fn single_worker_round_trips() {
        let input = sample(200_000);
        let blob = make_bgzf(&input, 6);
        assert_eq!(read_mt(&blob, 1), input);
    }

    /// Store-only (level 0) multi-block streams exercise the stored-block fast path in the
    /// workers.
    #[test]
    fn store_only_multi_block_round_trips() {
        let input = sample(250_000);
        let blob = make_bgzf(&input, 0);
        for workers in [1usize, 3] {
            assert_eq!(read_mt(&blob, workers), input);
        }
    }

    /// Empty input (only the EOF marker) reads as zero bytes, not an error.
    #[test]
    fn empty_stream_reads_nothing() {
        let blob = make_bgzf(b"", 6);
        assert!(read_mt(&blob, 4).is_empty());
    }

    /// Small `read` buffers must reassemble the stream correctly across block boundaries.
    #[test]
    fn tiny_reads_reassemble_stream() {
        let input = sample(150_000);
        let blob = make_bgzf(&input, 6);
        let mut reader =
            MultithreadedReader::with_worker_count(NonZero::new(4).unwrap(), owned(&blob));
        let mut out = vec![];
        let mut byte = [0u8; 1];
        while reader.read(&mut byte).unwrap() == 1 {
            out.push(byte[0]);
        }
        assert_eq!(out, input);
    }

    /// A stream truncated partway through a block must surface an error through `read`, the
    /// same as the single-threaded reader (not be silently treated as EOF).
    #[test]
    fn truncated_block_errors() {
        let input = sample(200_000);
        let blob = make_bgzf(&input, 6);
        // Cut inside the final data block (after the EOF-free portion): drop the trailing
        // EOF marker and part of the last block.
        let truncated = &blob[..blob.len() - crate::BGZF_EOF.len() - 20];

        let mut reader =
            MultithreadedReader::with_worker_count(NonZero::new(4).unwrap(), owned(truncated));
        let mut out = vec![];
        assert!(
            reader.read_to_end(&mut out).is_err(),
            "a truncated trailing block must error, not read as EOF"
        );
    }

    /// A corrupt block header must surface as an error.
    #[test]
    fn corrupt_header_errors() {
        let input = sample(120_000);
        let mut blob = make_bgzf(&input, 6);
        // Clobber the BC subfield identifier in the first block's header.
        blob[12] = b'X';

        let mut reader =
            MultithreadedReader::with_worker_count(NonZero::new(2).unwrap(), owned(&blob));
        let mut out = vec![];
        assert!(reader.read_to_end(&mut out).is_err());
    }

    /// A corrupt block payload (bad CRC) must be rejected — CRC is verified in the workers.
    #[test]
    fn corrupt_payload_errors() {
        let input = sample(80_000);
        let mut blob = make_bgzf(&input, 6);
        // Flip a byte inside the first block's compressed payload.
        blob[BGZF_HEADER_SIZE + 2] ^= 0xff;

        let mut reader =
            MultithreadedReader::with_worker_count(NonZero::new(4).unwrap(), owned(&blob));
        let mut out = vec![];
        assert!(reader.read_to_end(&mut out).is_err());
    }

    /// Dropping the reader mid-stream (without consuming everything) must not deadlock or
    /// leak threads — the pipeline shuts down cleanly.
    #[test]
    fn early_drop_is_clean() {
        let input = sample(500_000); // many blocks so plenty remain unread
        let blob = make_bgzf(&input, 6);
        let mut reader =
            MultithreadedReader::with_worker_count(NonZero::new(4).unwrap(), owned(&blob));
        let mut small = [0u8; 64];
        let _ = reader.read(&mut small).unwrap();
        drop(reader); // must return promptly
    }

    /// `finish` returns the inner reader and joins cleanly.
    #[test]
    fn finish_returns_inner() {
        let input = sample(100_000);
        let blob = make_bgzf(&input, 6);
        let mut reader = MultithreadedReader::new(owned(&blob));
        let mut out = vec![];
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, input);
        reader.finish().expect("finish should join cleanly");
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// For arbitrary input, level, and worker count, the multithreaded reader must agree
        /// with the single-threaded reader (and with the original input).
        #[test]
        fn proptest_mt_reader_matches_serial(
            input in prop::collection::vec(any::<u8>(), 1..100_000usize),
            comp_level in 0..=12u8,
            workers in 1usize..=4,
        ) {
            let blob = make_bgzf(&input, comp_level);

            let mut serial = vec![];
            Reader::new(blob.as_slice()).read_to_end(&mut serial).unwrap();
            prop_assert_eq!(&serial, &input);

            let mut mt = vec![];
            MultithreadedReader::with_worker_count(NonZero::new(workers).unwrap(), owned(&blob))
                .read_to_end(&mut mt)
                .unwrap();
            prop_assert_eq!(mt, input);
        }
    }
}
