//! Real-time spectrum tap: a `Source` wrapper that copies the samples flowing
//! to the output device into a shared ring buffer, plus the FFT that turns
//! those samples into frequency bands.
//!
//! The writer runs on the audio thread, so it never locks: samples are stored
//! as `AtomicU32` bit patterns behind a monotonically increasing write counter.
//! A reader can therefore tear by a sample or two under contention, which is
//! invisible in a visualizer and far cheaper than making the audio thread wait.
//!
//! The FFT is a plain iterative radix-2 — a 4096-point transform costs well
//! under a millisecond, which is nothing next to a frame, and it keeps the
//! crate dependency-free.

use std::f32::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

/// Samples fed to one FFT. Must be a power of two.
///
/// 4096 gives ~11Hz bins at 44.1kHz. Smaller windows are tempting (less
/// latency) but 1024 bins are 43Hz apart, which is wider than the bottom two
/// octaves of the band layout — every bass band would read the same bin and
/// the low end would move as one block.
pub const FFT_SIZE: usize = 4096;
/// Ring capacity — a few FFT windows of slack so a late reader still sees a
/// contiguous run of recent samples.
const RING: usize = 16_384;

/// Shared, lock-free window onto the most recent output samples (mono).
#[derive(Debug)]
pub struct SpectrumTap {
    ring: Box<[AtomicU32]>,
    /// Total samples ever written; also the ring's head.
    write: AtomicU64,
    sample_rate: AtomicU32,
}

impl SpectrumTap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ring: (0..RING).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicU64::new(0),
            sample_rate: AtomicU32::new(44_100),
        })
    }

    /// Sample rate of the source currently feeding the tap.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    /// Copy the newest `out.len()` samples (oldest first) and return the write
    /// counter. An unchanged counter means no audio has flowed since the last
    /// call — paused, stopped, or starved — so callers should decay to silence
    /// rather than hold the last frame.
    pub fn snapshot(&self, out: &mut [f32]) -> u64 {
        let head = self.write.load(Ordering::Acquire);
        let n = out.len().min(RING);
        for (k, slot) in out.iter_mut().enumerate().take(n) {
            let idx = head.wrapping_sub((n - k) as u64);
            *slot = f32::from_bits(self.ring[(idx % RING as u64) as usize].load(Ordering::Relaxed));
        }
        head
    }

    /// Feed a sample directly, for tests that need a tap without an engine.
    #[doc(hidden)]
    pub fn push_for_test(&self, sample: f32) {
        self.push(sample);
    }

    fn push(&self, sample: f32) {
        let head = self.write.load(Ordering::Relaxed);
        self.ring[(head % RING as u64) as usize].store(sample.to_bits(), Ordering::Relaxed);
        self.write.store(head.wrapping_add(1), Ordering::Release);
    }
}

/// Source wrapper that mirrors every sample into a `SpectrumTap`, downmixing
/// to mono so the FFT sees one signal rather than interleaved channels.
pub(crate) struct Tap<S> {
    inner: S,
    tap: Arc<SpectrumTap>,
    /// Partial frame accumulator for the downmix.
    acc: f32,
    acc_n: u16,
}

impl<S: Source> Tap<S> {
    pub(crate) fn new(inner: S, tap: Arc<SpectrumTap>) -> Self {
        tap.sample_rate
            .store(inner.sample_rate().get(), Ordering::Relaxed);
        Self {
            inner,
            tap,
            acc: 0.,
            acc_n: 0,
        }
    }
}

impl<S: Source> Iterator for Tap<S> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        let sample = self.inner.next()?;
        self.acc += sample;
        self.acc_n += 1;
        if self.acc_n >= self.inner.channels().get() {
            self.tap.push(self.acc / self.acc_n as f32);
            self.acc = 0.;
            self.acc_n = 0;
        }
        Some(sample)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tap<S> {
    #[inline]
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    #[inline]
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    #[inline]
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    #[inline]
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}

/// Lowest/highest frequency the bands span. Below ~30Hz is rumble no speaker
/// reproduces; above ~16kHz most material (and most ears) has nothing.
const MIN_HZ: f32 = 30.;
const MAX_HZ: f32 = 16_000.;
/// Magnitudes below this (in dB relative to full scale) read as silence.
const FLOOR_DB: f32 = -70.;

/// Window `samples`, run the FFT, and reduce the result to `bands.len()`
/// log-spaced frequency bands normalized to roughly 0..1.
///
/// `samples` is truncated/zero-padded to [`FFT_SIZE`]. Bands are log-spaced
/// because pitch is: linear bins would spend three quarters of the display on
/// the top two octaves, where music has almost no energy.
pub fn analyze(samples: &[f32], bands: &mut [f32], sample_rate: u32) {
    let mut re = [0f32; FFT_SIZE];
    let mut im = [0f32; FFT_SIZE];
    let n = samples.len().min(FFT_SIZE);
    let offset = samples.len().saturating_sub(FFT_SIZE);
    for i in 0..n {
        // Hann window — without it, the ends of the buffer act like a step and
        // smear energy across every bin.
        let w = 0.5 - 0.5 * (2. * PI * i as f32 / FFT_SIZE as f32).cos();
        re[i] = samples[offset + i] * w;
    }
    fft(&mut re, &mut im);

    let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
    let bands_len = bands.len();
    let ratio = (MAX_HZ / MIN_HZ).powf(1. / bands_len as f32);
    // Amplitude of a full-scale sine after windowing, used to put 0 dB at the
    // top of the scale instead of somewhere that depends on FFT_SIZE.
    let full_scale = FFT_SIZE as f32 / 4.;

    let mut lo_hz = MIN_HZ;
    for (b, out) in bands.iter_mut().enumerate() {
        let hi_hz = MIN_HZ * ratio.powi(b as i32 + 1);
        let lo_bin = ((lo_hz / bin_hz).floor() as usize).max(1);
        let hi_bin = ((hi_hz / bin_hz).ceil() as usize).min(FFT_SIZE / 2 - 1);
        // Peak, not mean: a band spanning many bins would otherwise dilute a
        // sharp tone into the noise around it.
        let mut peak = 0f32;
        for bin in lo_bin..=hi_bin.max(lo_bin) {
            let mag = (re[bin] * re[bin] + im[bin] * im[bin]).sqrt();
            peak = peak.max(mag);
        }
        let db = 20. * (peak / full_scale).max(1e-9).log10();
        *out = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0., 1.);
        lo_hz = hi_hz;
    }
}

/// In-place iterative radix-2 Cooley-Tukey FFT. `re`/`im` must be the same
/// power-of-two length.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let ang = -2. * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cur_r, mut cur_i) = (1f32, 0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cur_r - im[b] * cur_i;
                let ti = re[b] * cur_i + im[b] * cur_r;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next_r = cur_r * wr - cur_i * wi;
                cur_i = cur_r * wi + cur_i * wr;
                cur_r = next_r;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index of the loudest band.
    fn peak_band(bands: &[f32]) -> usize {
        bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap()
    }

    fn sine(freq: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2. * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn silence_is_floor() {
        let mut bands = [0f32; 32];
        analyze(&[0.; FFT_SIZE], &mut bands, 44_100);
        assert!(bands.iter().all(|b| *b == 0.), "{bands:?}");
    }

    #[test]
    fn tone_lands_in_the_band_containing_it() {
        let rate = 44_100;
        let mut bands = [0f32; 32];
        for freq in [110., 440., 3_000.] {
            analyze(&sine(freq, rate, FFT_SIZE), &mut bands, rate);
            let ratio: f32 = (MAX_HZ / MIN_HZ).powf(1. / bands.len() as f32);
            let expected = (freq / MIN_HZ).log(ratio).floor() as usize;
            let got = peak_band(&bands);
            assert!(
                got.abs_diff(expected) <= 1,
                "{freq}Hz: peak in band {got}, expected ~{expected} ({bands:?})"
            );
            assert!(bands[got] > 0.5, "{freq}Hz peaked at only {}", bands[got]);
        }
    }

    #[test]
    fn louder_signal_reads_higher() {
        let rate = 44_100;
        let loud = sine(440., rate, FFT_SIZE);
        let quiet: Vec<f32> = loud.iter().map(|s| s * 0.05).collect();
        let (mut a, mut b) = ([0f32; 32], [0f32; 32]);
        analyze(&loud, &mut a, rate);
        analyze(&quiet, &mut b, rate);
        let band = peak_band(&a);
        assert!(a[band] > b[band], "loud {} vs quiet {}", a[band], b[band]);
    }

    #[test]
    fn tap_snapshot_returns_newest_samples_oldest_first() {
        let tap = SpectrumTap::new();
        for i in 0..10 {
            tap.push(i as f32);
        }
        let mut out = [0f32; 4];
        let head = tap.snapshot(&mut out);
        assert_eq!(head, 10);
        assert_eq!(out, [6., 7., 8., 9.]);
    }

    #[test]
    fn tap_snapshot_survives_ring_wraparound() {
        let tap = SpectrumTap::new();
        for i in 0..(RING + 3) {
            tap.push(i as f32);
        }
        let mut out = [0f32; 3];
        tap.snapshot(&mut out);
        assert_eq!(out, [RING as f32, (RING + 1) as f32, (RING + 2) as f32]);
    }
}
