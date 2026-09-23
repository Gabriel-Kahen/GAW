//! Bounded packet consumption, including partial packets between output callbacks.

use super::{InputBlock, Ordering, Shared};

#[derive(Debug, Default)]
pub(super) struct InputQueueReader {
    block: Option<InputBlock>,
    position: usize,
}

impl InputQueueReader {
    fn ready(&mut self, shared: &Shared, budget: &mut usize) -> bool {
        if self.block.is_some() {
            return true;
        }
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        self.block = shared.receiver.try_recv().ok();
        self.position = 0;
        self.block.is_some()
    }

    fn advance(&mut self, shared: &Shared, frames: usize, dropped: bool) {
        self.position += frames;
        shared.queued_frames.fetch_sub(frames, Ordering::AcqRel);
        if dropped {
            shared
                .dropped_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
        }
        if self
            .block
            .as_ref()
            .is_some_and(|block| self.position == block.len)
        {
            self.block = None;
            self.position = 0;
        }
    }

    pub(super) fn trim_backlog(&mut self, shared: &Shared, keep: usize, budget: &mut usize) {
        // A producer may publish during trimming, but cannot evict existing
        // packets under us. Neither callback waits for this bounded claim.
        if shared
            .eviction_claim
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let generation = shared.publication_generation.load(Ordering::Acquire);
        let buffered = shared.available_frames();
        if shared.in_flight_frames.load(Ordering::Acquire) == 0
            && shared.publication_generation.load(Ordering::Acquire) == generation
        {
            self.discard(shared, buffered.saturating_sub(keep), budget);
        }
        shared.eviction_claim.store(false, Ordering::Release);
    }

    pub(super) fn discard(&mut self, shared: &Shared, mut frames: usize, budget: &mut usize) {
        while frames > 0 && self.ready(shared, budget) {
            let available = self.block.as_ref().unwrap().len - self.position;
            let take = frames.min(available);
            self.advance(shared, take, true);
            frames -= take;
        }
    }

    pub(super) fn read(
        &mut self,
        shared: &Shared,
        identity: (u64, u64),
        output: &mut [f32],
        budget: &mut usize,
    ) -> usize {
        let mut written = 0;
        while written < output.len() && self.ready(shared, budget) {
            let block = self.block.as_ref().unwrap();
            let available = block.len - self.position;
            if (block.epoch, block.stream) != identity {
                self.advance(shared, available, true);
                continue;
            }
            let take = available.min(output.len() - written);
            output[written..written + take]
                .copy_from_slice(&block.samples[self.position..self.position + take]);
            self.advance(shared, take, false);
            written += take;
        }
        written
    }

    pub(super) fn release(&mut self, shared: &Shared) {
        if let Some(block) = self.block.take() {
            shared
                .queued_frames
                .fetch_sub(block.len - self.position, Ordering::AcqRel);
        }
    }
}

/// Maximum backlog from two periodic callbacks at arbitrary relative phase.
/// Equal-sized callbacks need one buffer; unequal sizes need the larger buffer
/// plus the largest possible remainder. This retains a complete large input
/// callback when output callbacks are smaller without retaining two large blocks.
pub(super) fn backlog_limit(input: usize, output: usize, resampling: bool) -> usize {
    if resampling {
        // Fractional callback sizes have no common integer alignment.
        return input.saturating_add(output).saturating_sub(1);
    }
    let (mut a, mut b) = (input, output);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    input.saturating_add(output).saturating_sub(a)
}
