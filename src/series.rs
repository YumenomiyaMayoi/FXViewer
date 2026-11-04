//! OHLC aggregation.
//!
//! The whole idea of this app lives here: a resource metric sampled several
//! times per second is folded into candles, so that a single candle carries
//! open / high / low / close plus the flow that happened during that bucket.

use std::collections::VecDeque;

/// Seconds covered by one candle, per selectable timeframe.
pub const TIMEFRAMES: [u32; 5] = [1, 5, 15, 60, 300];
pub const TF_LABELS: [&str; 5] = ["1s", "5s", "15s", "1m", "5m"];

/// Candles kept per timeframe per symbol. 1800 x 1s = 30 min of history,
/// 1800 x 5m = 150 h. Memory stays flat at a few hundred KB.
pub const HISTORY: usize = 1800;

#[derive(Clone, Copy, Default, Debug)]
pub struct Candle {
    pub t: i64,
    pub o: f32,
    pub h: f32,
    pub l: f32,
    pub c: f32,
    /// Accumulated flow (network bytes) inside this bucket.
    pub v: f32,
}

/// One timeframe's ring buffer. `dropped` counts candles evicted from the
/// front so the chart can keep a scrolled-back view anchored to real data
/// instead of to an index that shifts every second.
pub struct Frame {
    candles: VecDeque<Candle>,
    dropped: u64,
}

pub struct Series {
    frames: Vec<Frame>,
}

impl Default for Series {
    fn default() -> Self {
        Self::new()
    }
}

impl Series {
    pub fn new() -> Self {
        Self {
            frames: (0..TIMEFRAMES.len())
                .map(|_| Frame {
                    candles: VecDeque::with_capacity(HISTORY + 2),
                    dropped: 0,
                })
                .collect(),
        }
    }

    /// Feed one raw sample. `ts` is a unix timestamp in seconds.
    pub fn push(&mut self, ts: i64, value: f32, flow: f32) {
        for i in 0..self.frames.len() {
            let width = TIMEFRAMES[i] as i64;
            let bucket = ts - ts.rem_euclid(width);
            let frame = &mut self.frames[i];

            if let Some(last) = frame.candles.back_mut() {
                if last.t == bucket {
                    last.c = value;
                    if value > last.h {
                        last.h = value;
                    }
                    if value < last.l {
                        last.l = value;
                    }
                    last.v += flow;
                    continue;
                }
                if last.t > bucket {
                    // Out of order sample (clock went backwards); ignore.
                    continue;
                }
            }

            frame.candles.push_back(Candle {
                t: bucket,
                o: value,
                h: value,
                l: value,
                c: value,
                v: flow,
            });
            while frame.candles.len() > HISTORY {
                frame.candles.pop_front();
                frame.dropped += 1;
            }
        }
    }

    pub fn candles(&self, tf: usize) -> &VecDeque<Candle> {
        &self.frames[tf.min(self.frames.len() - 1)].candles
    }

    /// Drops every candle. Used when the same slot starts tracking a different
    /// source — splicing two instruments into one line would corrupt the MAs.
    pub fn clear(&mut self) {
        for frame in &mut self.frames {
            frame.candles.clear();
            frame.dropped = 0;
        }
    }

    /// How many candles this timeframe has already pushed out of the buffer.
    pub fn dropped(&self, tf: usize) -> u64 {
        self.frames[tf.min(self.frames.len() - 1)].dropped
    }

    pub fn last(&self, tf: usize) -> Option<Candle> {
        self.candles(tf).back().copied()
    }

    pub fn prev_close(&self, tf: usize) -> Option<f32> {
        let dq = self.candles(tf);
        if dq.len() < 2 {
            return None;
        }
        dq.get(dq.len() - 2).map(|c| c.c)
    }

    /// Simple moving average of closes ending at `idx`.
    pub fn ma(dq: &VecDeque<Candle>, idx: usize, period: usize) -> Option<f32> {
        if period == 0 || idx + 1 < period {
            return None;
        }
        let mut sum = 0.0f32;
        for k in 0..period {
            sum += dq.get(idx - k)?.c;
        }
        Some(sum / period as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_roll_over_and_track_extremes() {
        let mut s = Series::new();
        // Four samples inside the same 1s bucket.
        s.push(1000, 10.0, 1.0);
        s.push(1000, 25.0, 1.0);
        s.push(1000, 5.0, 1.0);
        s.push(1000, 12.0, 1.0);
        let c = s.last(0).unwrap();
        assert_eq!(c.o, 10.0);
        assert_eq!(c.h, 25.0);
        assert_eq!(c.l, 5.0);
        assert_eq!(c.c, 12.0);
        assert_eq!(c.v, 4.0);
        assert_eq!(s.candles(0).len(), 1);

        // Next second opens a fresh candle at the last value.
        s.push(1001, 30.0, 1.0);
        let c = s.last(0).unwrap();
        assert_eq!(c.o, 30.0);
        assert_eq!(s.candles(0).len(), 2);

        // The 5s frame absorbed all five samples into one candle.
        let wide = s.candles(1).back().unwrap();
        assert_eq!(wide.o, 10.0);
        assert_eq!(wide.c, 30.0);
        assert_eq!(wide.h, 30.0);
        assert_eq!(wide.l, 5.0);
    }

    #[test]
    fn history_is_bounded() {
        let mut s = Series::new();
        for i in 0..(HISTORY + 50) {
            s.push(10_000 + i as i64, i as f32, 0.0);
        }
        assert_eq!(s.candles(0).len(), HISTORY);
        assert_eq!(s.dropped(0), 50);
    }

    #[test]
    fn moving_average_needs_full_window() {
        let mut s = Series::new();
        for i in 0..10 {
            s.push(2000 + i as i64, i as f32, 0.0);
        }
        let dq = s.candles(0);
        assert!(Series::ma(dq, 3, 5).is_none());
        let ma5 = Series::ma(dq, 9, 5).unwrap();
        assert!((ma5 - 7.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod clear_tests {
    use super::*;

    #[test]
    fn clear_empties_every_timeframe() {
        let mut s = Series::new();
        for i in 0..200 {
            s.push(10_000 + i, i as f32, 1.0);
        }
        assert!(!s.candles(0).is_empty());
        s.clear();
        for tf in 0..TIMEFRAMES.len() {
            assert!(s.candles(tf).is_empty());
            assert_eq!(s.dropped(tf), 0);
        }
    }

    #[test]
    fn replaying_history_rebuilds_the_same_last_close() {
        let mut a = Series::new();
        let mut hist = Vec::new();
        for i in 0..120i64 {
            let v = (i % 17) as f32;
            a.push(20_000 + i, v, 0.5);
            hist.push((20_000 + i, v, 0.5));
        }
        let before = a.candles(0).back().map(|c| (c.t, c.c));

        let mut b = Series::new();
        for (t, v, f) in hist {
            b.push(t, v, f);
        }
        assert_eq!(before, b.candles(0).back().map(|c| (c.t, c.c)));
    }
}
