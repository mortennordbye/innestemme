use std::time::{Duration, Instant};

use voice_proto::MAX_PAYLOAD;

/// Slots in the reorder window. 64 x 20 ms covers 1.28 s, far beyond any useful depth.
const SLOTS: usize = 64;

struct Slot {
    seq: u32,
    len: usize,
    filled: bool,
    arrival: Instant,
    data: [u8; MAX_PAYLOAD],
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    pub late: u64,
    pub duplicate: u64,
    pub lost: u64,
    pub resync: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Accepted,
    /// Arrived after its position was already released or declared lost.
    Late,
    Duplicate,
    /// Sequence jumped beyond the window; the buffer restarted at this packet.
    Resync,
}

pub enum Popped<'a> {
    Packet { seq: u32, payload: &'a [u8], waited: Duration },
    /// Position given up on; the caller should conceal one packet of audio.
    Missing { seq: u32 },
}

/// Sequence-ordered reorder buffer with preallocated storage.
///
/// In-order packets are released immediately, so the buffer adds no latency on a clean link.
/// A gap holds the stream until `depth` later packets have arrived, then the missing position is
/// declared lost. `depth` is therefore the reordering tolerance and the worst-case added delay
/// (`depth` x 20 ms), not a constant playout delay.
pub struct JitterBuffer {
    slots: Box<[Slot]>,
    depth: u32,
    /// Next sequence number to release. `None` until the first packet.
    next: Option<u32>,
    highest: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(depth: u32) -> Self {
        let now = Instant::now();
        let slots = (0..SLOTS)
            .map(|_| Slot { seq: 0, len: 0, filled: false, arrival: now, data: [0; MAX_PAYLOAD] })
            .collect();
        Self { slots, depth: depth.min(SLOTS as u32 - 1), next: None, highest: 0, stats: JitterStats::default() }
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn reset(&mut self) {
        self.slots.iter_mut().for_each(|s| s.filled = false);
        self.next = None;
        self.stats = JitterStats::default();
    }

    /// Resets and expects the stream to begin at `seq`. Without this the first packet to arrive
    /// defines the start, so a reordered opening would drop the true first packet as late.
    pub fn reset_at(&mut self, seq: u32) {
        self.reset();
        self.next = Some(seq);
        self.highest = seq;
    }

    /// Stores a packet. Payloads longer than `MAX_PAYLOAD` are truncated; the wire parser rejects
    /// those before they get here.
    pub fn push(&mut self, seq: u32, payload: &[u8], now: Instant) -> Pushed {
        self.stats.received += 1;
        let next = *self.next.get_or_insert_with(|| {
            self.highest = seq;
            seq
        });
        let ahead = seq.wrapping_sub(next) as i32;
        if ahead < 0 {
            self.stats.late += 1;
            return Pushed::Late;
        }
        let mut outcome = Pushed::Accepted;
        if ahead as usize >= SLOTS {
            self.slots.iter_mut().for_each(|s| s.filled = false);
            self.next = Some(seq);
            self.highest = seq;
            self.stats.resync += 1;
            outcome = Pushed::Resync;
        }
        let slot = &mut self.slots[seq as usize % SLOTS];
        if slot.filled && slot.seq == seq {
            self.stats.duplicate += 1;
            return Pushed::Duplicate;
        }
        let len = payload.len().min(MAX_PAYLOAD);
        slot.data[..len].copy_from_slice(&payload[..len]);
        slot.seq = seq;
        slot.len = len;
        slot.filled = true;
        slot.arrival = now;
        if (seq.wrapping_sub(self.highest) as i32) > 0 {
            self.highest = seq;
        }
        outcome
    }

    /// Releases the next position if it is available or has been given up on.
    pub fn pop(&mut self, now: Instant) -> Option<Popped<'_>> {
        self.pop_with_depth(now, self.depth)
    }

    /// Like `pop` but gives up on gaps immediately. Used to drain the tail at end of stream.
    pub fn pop_flush(&mut self, now: Instant) -> Option<Popped<'_>> {
        self.pop_with_depth(now, 0)
    }

    fn pop_with_depth(&mut self, now: Instant, depth: u32) -> Option<Popped<'_>> {
        let next = self.next?;
        let slot = &mut self.slots[next as usize % SLOTS];
        if slot.filled && slot.seq == next {
            slot.filled = false;
            self.next = Some(next.wrapping_add(1));
            let waited = now.saturating_duration_since(slot.arrival);
            return Some(Popped::Packet { seq: next, payload: &slot.data[..slot.len], waited });
        }
        let newer = self.highest.wrapping_sub(next) as i32;
        if newer > 0 && newer as u32 >= depth.max(1) {
            self.next = Some(next.wrapping_add(1));
            self.stats.lost += 1;
            return Some(Popped::Missing { seq: next });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(jb: &mut JitterBuffer, now: Instant) -> Vec<Result<u32, u32>> {
        let mut out = Vec::new();
        while let Some(p) = jb.pop(now) {
            out.push(match p {
                Popped::Packet { seq, payload, .. } => {
                    assert_eq!(payload, &seq.to_le_bytes());
                    Ok(seq)
                }
                Popped::Missing { seq } => Err(seq),
            });
        }
        out
    }

    fn push(jb: &mut JitterBuffer, seq: u32, now: Instant) -> Pushed {
        jb.push(seq, &seq.to_le_bytes(), now)
    }

    #[test]
    fn in_order_is_released_immediately() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(2);
        for seq in 10..14 {
            assert_eq!(push(&mut jb, seq, now), Pushed::Accepted);
            assert_eq!(drain(&mut jb, now), vec![Ok(seq)]);
        }
    }

    #[test]
    fn reorder_within_depth_is_repaired() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(2);
        push(&mut jb, 0, now);
        assert_eq!(drain(&mut jb, now), vec![Ok(0)]);
        push(&mut jb, 2, now);
        assert_eq!(drain(&mut jb, now), vec![]);
        push(&mut jb, 1, now);
        assert_eq!(drain(&mut jb, now), vec![Ok(1), Ok(2)]);
        assert_eq!(jb.stats().lost, 0);
    }

    #[test]
    fn loss_is_declared_after_depth_and_late_arrival_dropped() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(2);
        push(&mut jb, 0, now);
        drain(&mut jb, now);
        push(&mut jb, 2, now);
        assert_eq!(drain(&mut jb, now), vec![]);
        push(&mut jb, 3, now);
        assert_eq!(drain(&mut jb, now), vec![Err(1), Ok(2), Ok(3)]);
        assert_eq!(push(&mut jb, 1, now), Pushed::Late);
        assert_eq!(jb.stats(), JitterStats { received: 4, late: 1, duplicate: 0, lost: 1, resync: 0 });
    }

    #[test]
    fn known_start_repairs_reordered_opening() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(2);
        jb.reset_at(0);
        push(&mut jb, 1, now);
        assert_eq!(drain(&mut jb, now), vec![]);
        assert_eq!(push(&mut jb, 0, now), Pushed::Accepted);
        assert_eq!(drain(&mut jb, now), vec![Ok(0), Ok(1)]);
    }

    #[test]
    fn duplicates_and_wraparound() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(2);
        push(&mut jb, u32::MAX, now);
        assert_eq!(drain(&mut jb, now), vec![Ok(u32::MAX)]);
        push(&mut jb, 1, now);
        assert_eq!(push(&mut jb, 1, now), Pushed::Duplicate);
        push(&mut jb, 0, now);
        assert_eq!(drain(&mut jb, now), vec![Ok(0), Ok(1)]);
    }

    #[test]
    fn large_jump_resyncs_and_flush_drains_tail() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new(4);
        push(&mut jb, 0, now);
        drain(&mut jb, now);
        assert_eq!(push(&mut jb, 5_000, now), Pushed::Resync);
        assert_eq!(drain(&mut jb, now), vec![Ok(5_000)]);
        push(&mut jb, 5_002, now);
        assert_eq!(drain(&mut jb, now), vec![]);
        assert!(matches!(jb.pop_flush(now), Some(Popped::Missing { seq: 5_001 })));
        assert!(matches!(jb.pop_flush(now), Some(Popped::Packet { seq: 5_002, .. })));
        assert!(jb.pop_flush(now).is_none());
    }
}
