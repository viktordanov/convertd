//! Reusable 64 KiB buffers for the streams of one request: the upload's
//! pieces on their way to the sink, and the response pump's chunks.

use bytes::BytesMut;

/// The buffers of one stream, reused. A chunk sent on shares the
/// allocation of its slot. When its reader (hyper, or the upload sink)
/// drops the chunk, `try_reclaim` gives the slot its allocation back. So
/// a stream allocates one slot per chunk in flight, once, not one buffer
/// per chunk. A response's pump holds at most `BODY_QUEUE_CHUNKS` + 2
/// when the reader drops each chunk at once, and 13 behind hyper for an
/// 8 MiB PDF over loopback.
#[derive(Default)]
pub(super) struct Chunks {
    pub(super) slots: Vec<BytesMut>,
}

impl Chunks {
    /// The index of an empty slot with room for `len` bytes that no chunk
    /// in flight uses: room left after a short chunk, or the whole buffer
    /// once its chunks are gone. A new slot of `len` bytes when none is
    /// free, so a PDF shorter than a chunk gets no 64 KiB.
    pub(super) fn free(&mut self, len: usize) -> usize {
        if let Some(index) = self
            .slots
            .iter_mut()
            .position(|slot| slot.is_empty() && slot.try_reclaim(len))
        {
            return index;
        }
        self.slots.push(BytesMut::with_capacity(len));
        self.slots.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_CHUNK: usize = 64 * 1024;

    #[test]
    fn a_slot_is_reused_once_hyper_drops_its_chunk() {
        let mut chunks = Chunks::default();
        let first = chunks.free(READ_CHUNK);
        chunks.slots[first].resize(READ_CHUNK, 1);
        let chunk = chunks.slots[first].split().freeze();
        let address = chunk.as_ptr();
        // The chunk is in flight: its slot is busy.
        let second = chunks.free(READ_CHUNK);
        assert_ne!(second, first);
        assert_eq!(
            chunks.free(READ_CHUNK),
            second,
            "an empty free slot is reused"
        );
        drop(chunk);
        assert_eq!(chunks.free(READ_CHUNK), first);
        chunks.slots[first].resize(READ_CHUNK, 0);
        assert_eq!(chunks.slots[first].as_ptr(), address, "same allocation");
        assert_eq!(chunks.slots.len(), 2);
        // A short read gets a buffer of its size when no slot is free.
        let _busy = chunks.slots[first].split().freeze();
        chunks.slots[second].resize(READ_CHUNK, 0);
        let _also_busy = chunks.slots[second].split().freeze();
        let short = chunks.free(1000);
        assert_eq!(short, 2);
        assert_eq!(chunks.slots[short].capacity(), 1000);
    }
}
