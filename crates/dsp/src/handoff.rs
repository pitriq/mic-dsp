//! Wait-free handoff of the latest value between two threads: a triple buffer.
//!
//! The writer fills its own slot and swaps it with the middle slot. The reader swaps its slot
//! with the middle one when the middle holds something new. Each side owns its slot outright
//! between swaps, so neither waits and no value is read while it is written. This file holds
//! the crate's only `unsafe`.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Low bits: the index of the middle slot.
const INDEX: u8 = 0b011;
/// Set when the middle slot holds a value the reader has not taken.
const FRESH: u8 = 0b100;

struct Shared<T> {
    slots: [UnsafeCell<T>; 3],
    middle: AtomicU8,
}

// SAFETY: a slot is only touched by the side that currently owns its index, and ownership
// moves through the atomic swap on `middle`.
unsafe impl<T: Send> Sync for Shared<T> {}

pub(crate) struct Tx<T> {
    shared: Arc<Shared<T>>,
    slot: u8,
}

pub(crate) struct Rx<T> {
    shared: Arc<Shared<T>>,
    slot: u8,
}

/// A channel that only ever holds the latest value. `Rx` starts out holding `init`.
pub(crate) fn latest<T: Copy + Send>(init: T) -> (Tx<T>, Rx<T>) {
    let shared = Arc::new(Shared {
        slots: [
            UnsafeCell::new(init),
            UnsafeCell::new(init),
            UnsafeCell::new(init),
        ],
        middle: AtomicU8::new(1),
    });
    (
        Tx {
            shared: shared.clone(),
            slot: 0,
        },
        Rx { shared, slot: 2 },
    )
}

impl<T: Copy + Send> Tx<T> {
    /// Wait-free. Replaces whatever the reader has not taken yet.
    pub(crate) fn publish(&mut self, value: T) {
        // SAFETY: `self.slot` belongs to the writer until the swap below hands it over.
        unsafe { *self.shared.slots[self.slot as usize].get() = value };
        let previous = self.shared.middle.swap(self.slot | FRESH, Ordering::AcqRel);
        self.slot = previous & INDEX;
    }
}

impl<T: Copy + Send> Rx<T> {
    /// Wait-free. The newest value, if one arrived since the last call.
    pub(crate) fn fresh(&mut self) -> Option<T> {
        if self.shared.middle.load(Ordering::Acquire) & FRESH == 0 {
            return None;
        }
        let previous = self.shared.middle.swap(self.slot, Ordering::AcqRel);
        self.slot = previous & INDEX;
        Some(self.current())
    }

    /// Wait-free. The newest value, whether or not it was seen before.
    pub(crate) fn latest(&mut self) -> T {
        self.fresh().unwrap_or_else(|| self.current())
    }

    fn current(&self) -> T {
        // SAFETY: `self.slot` belongs to the reader until its next swap.
        unsafe { *self.shared.slots[self.slot as usize].get() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_sees_the_initial_value_then_only_the_newest() {
        let (mut tx, mut rx) = latest(7u32);
        assert_eq!(rx.fresh(), None);
        assert_eq!(rx.latest(), 7);
        tx.publish(1);
        tx.publish(2);
        tx.publish(3);
        assert_eq!(rx.fresh(), Some(3));
        assert_eq!(rx.fresh(), None);
        assert_eq!(rx.latest(), 3);
    }

    /// One producer, one consumer. A torn or stale read would show up as a block whose words
    /// disagree or as a counter that goes backwards.
    #[test]
    fn stress_single_producer_single_consumer() {
        const LAST: u64 = 2_000_000;
        let (mut tx, mut rx) = latest([0u64; 16]);
        let producer = std::thread::spawn(move || {
            for n in 1..=LAST {
                tx.publish([n; 16]);
            }
        });
        let (mut seen, mut distinct, mut reads) = (0u64, 0u64, 0u64);
        while seen < LAST {
            // Alternate the two ways of reading.
            reads += 1;
            let block = if reads % 2 == 0 {
                rx.latest()
            } else if let Some(block) = rx.fresh() {
                assert!(
                    block[0] > seen,
                    "fresh returned {} again after {seen}",
                    block[0]
                );
                block
            } else {
                continue;
            };
            assert!(
                block.iter().all(|word| *word == block[0]),
                "torn read: {block:?}"
            );
            assert!(block[0] >= seen, "went from {seen} back to {}", block[0]);
            distinct += u64::from(block[0] > seen);
            seen = block[0];
        }
        producer.join().unwrap();
        assert_eq!(rx.fresh(), None);
        assert!(distinct > 1);
    }
}
