// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! 16-bit PCM for a head unit's player of another format, and two streams in one player: the iPhone's stream is
//! converted to the rate and channels the player was started with, and streams that share a player are mixed, so that
//! the player is neither started again nor fed two streams one packet after the other.

use std::collections::VecDeque;

use crate::sound::Format;

/// Converts one stream of 16-bit little-endian interleaved PCM from one format to another: channels first, then the
/// rate, by linear interpolation that carries over from one packet to the next.
#[derive(Clone, Debug)]
pub struct Converter {
    from: Format,
    to: Format,
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls, in input frames from the last frame of the packet before.
    position: f64,
    /// The last frame of the packet before, in the output's channels.
    last: Option<Vec<f32>>,
}

impl Converter {
    pub fn new(from: Format, to: Format) -> Converter {
        Converter {
            from,
            to,
            step: f64::from(from.sample_rate.max(1)) / f64::from(to.sample_rate.max(1)),
            position: 0.0,
            last: None,
        }
    }

    /// The packet `pcm` in the output's format.
    pub fn convert(&mut self, pcm: &[u8]) -> Vec<u8> {
        if self.from == self.to {
            return pcm.to_vec();
        }
        let channels_in = usize::from(self.from.channels.max(1));
        let channels_out = usize::from(self.to.channels.max(1));
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let mut frames: Vec<Vec<f32>> = self.last.take().into_iter().collect();
        let carried = frames.len();
        frames.extend(
            samples
                .chunks_exact(channels_in)
                .map(|frame| remix(frame, channels_out)),
        );
        if frames.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let last = frames.len() - 1;
        // A first packet starts at its first frame; later ones go on from the frame carried over.
        let mut position = if carried == 0 { 0.0 } else { self.position };
        while position <= last as f64 {
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            let next = &frames[(index + 1).min(last)];
            for (channel, &sample) in frames[index].iter().enumerate() {
                let value = sample + (next[channel] - sample) * fraction;
                out.extend_from_slice(
                    &(value.round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                );
            }
            position += self.step;
            if index == last {
                break;
            }
        }
        self.position = position - last as f64;
        self.last = frames.pop();
        out
    }
}

/// A frame of `frame.len()` channels in `channels` channels: one channel repeated, or the channels averaged into one,
/// or the first ones kept.
fn remix(frame: &[i16], channels: usize) -> Vec<f32> {
    let values: Vec<f32> = frame.iter().map(|&s| f32::from(s)).collect();
    match (values.len(), channels) {
        (n, m) if n == m => values,
        (1, m) => vec![values[0]; m],
        (n, 1) => vec![values.iter().sum::<f32>() / n as f32],
        (n, m) if n > m => values[..m].to_vec(),
        (_, m) => {
            let mut out = values.clone();
            out.resize(m, *values.last().unwrap_or(&0.0));
            out
        }
    }
}

/// What a stream that shares a player waits with until the stream that leads the player sends: its samples in the
/// player's format, at most a quarter of a second of them, the oldest dropped first.
#[derive(Clone, Debug, Default)]
pub struct Pending {
    samples: VecDeque<i16>,
}

impl Pending {
    /// Keeps `pcm` for the player of `format`.
    pub fn push(&mut self, pcm: &[u8], format: Format) {
        self.samples.extend(
            pcm.chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );
        let limit = (format.sample_rate as usize * usize::from(format.channels.max(1))) / 4;
        let limit = limit - limit % usize::from(format.channels.max(1));
        while self.samples.len() > limit {
            self.samples.pop_front();
        }
    }

    /// Mixes what waits into `pcm`, up to its length.
    pub fn mix_into(&mut self, pcm: &mut [u8]) {
        for sample in pcm.chunks_exact_mut(2) {
            let Some(waiting) = self.samples.pop_front() else {
                break;
            };
            let own = i16::from_le_bytes([sample[0], sample[1]]);
            sample.copy_from_slice(&own.saturating_add(waiting).to_le_bytes());
        }
    }

    /// What waits, as PCM, leaving none.
    pub fn take(&mut self) -> Vec<u8> {
        self.samples.drain(..).flat_map(i16::to_le_bytes).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(pcm: &[u8]) -> Vec<i16> {
        pcm.chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    const MONO_16K: Format = Format {
        sample_rate: 16000,
        channels: 1,
    };
    const MONO_48K: Format = Format {
        sample_rate: 48000,
        channels: 1,
    };
    const STEREO_48K: Format = Format {
        sample_rate: 48000,
        channels: 2,
    };

    #[test]
    fn the_same_format_passes() {
        let mut same = Converter::new(MONO_16K, MONO_16K);
        assert_eq!(same.convert(&pcm(&[1, 2, 3])), pcm(&[1, 2, 3]));
    }

    #[test]
    fn channels_are_repeated_or_averaged() {
        let mut up = Converter::new(MONO_48K, STEREO_48K);
        assert_eq!(
            samples(&up.convert(&pcm(&[100, -100]))),
            [100, 100, -100, -100]
        );
        let mut down = Converter::new(STEREO_48K, MONO_48K);
        assert_eq!(
            samples(&down.convert(&pcm(&[100, 300, -50, -150]))),
            [200, -100]
        );
    }

    #[test]
    fn the_rate_is_converted_across_packets() {
        // 48 kHz to 16 kHz: every third frame.
        let mut down = Converter::new(MONO_48K, MONO_16K);
        let ramp: Vec<i16> = (0..48).map(|i| i * 10).collect();
        let first = samples(&down.convert(&pcm(&ramp[..24])));
        let second = samples(&down.convert(&pcm(&ramp[24..])));
        let all: Vec<i16> = first.into_iter().chain(second).collect();
        assert_eq!(all, (0..16).map(|i| i * 30).collect::<Vec<i16>>());

        // 16 kHz to 48 kHz: three frames for each, interpolated, the packets joined without a step.
        let mut up = Converter::new(MONO_16K, MONO_48K);
        let first = samples(&up.convert(&pcm(&[0, 300])));
        let second = samples(&up.convert(&pcm(&[600, 900])));
        assert_eq!(first, [0, 100, 200, 300]);
        assert_eq!(second, [400, 500, 600, 700, 800, 900]);
        // A second's worth in gives a second's worth out, give or take a frame.
        let mut steady = Converter::new(
            Format {
                sample_rate: 44100,
                channels: 2,
            },
            MONO_16K,
        );
        let mut frames_out = 0;
        for _ in 0..100 {
            frames_out += steady.convert(&vec![0; 441 * 4]).len() / 2;
        }
        assert!((15999..=16001).contains(&frames_out), "{frames_out}");
    }

    #[test]
    fn what_waits_is_mixed_in() {
        let mut pending = Pending::default();
        assert!(pending.take().is_empty());
        pending.push(&pcm(&[10, 20, 30]), MONO_16K);
        let mut lead = pcm(&[1, 1, i16::MAX - 5, 1]);
        pending.mix_into(&mut lead);
        assert_eq!(samples(&lead), [11, 21, i16::MAX, 1]);
        assert!(pending.take().is_empty());
        // At most a quarter of a second waits, the newest kept.
        pending.push(&pcm(&vec![1; 4000]), MONO_16K);
        pending.push(&pcm(&[7, 8]), MONO_16K);
        let waiting = samples(&pending.take());
        assert_eq!(waiting.len(), 4000);
        assert_eq!(&waiting[3998..], [7, 8]);
    }
}
