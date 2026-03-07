use std::collections::VecDeque;
use rubato::{Fft, FixedSync, Resampler};
use rubato::audioadapter::Adapter;
use crate::messages::{AudioFrame, AudioSourceType};
use crate::config::AudioProcessingConfig;
use super::AudioProcessor;
use anyhow::Result;

// Custom adapter to bridge raw buffers to Rubato
struct PlanarBuffer<'a> {
    data: &'a [Vec<f32>],
    channels: usize,
    frames: usize,
}

impl<'a> Adapter<'a, f32> for PlanarBuffer<'a> {
    fn channels(&self) -> usize { self.channels }
    fn frames(&self) -> usize { self.frames }
    fn read_sample(&self, channel: usize, frame: usize) -> Option<f32> {
        self.data.get(channel).and_then(|ch| ch.get(frame)).copied()
    }
    unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f32 {
        *self.data.get_unchecked(channel).get_unchecked(frame)
    }
}

struct StreamState {
    resampler: Option<Fft<f32>>,
    source_rate: u32,
    input_buffer: VecDeque<f32>,
    output_queue: VecDeque<f32>,
    current_timestamp: u64,
    buffered_samples: u64,
    ready_queue: VecDeque<AudioFrame>,
}

impl StreamState {
    fn new(source_rate: u32, target_rate: u32, target_channels: u16, chunk_size: usize) -> Self {
        let resampler = if source_rate != target_rate {
            match Fft::<f32>::new(
                source_rate as usize,
                target_rate as usize,
                chunk_size,
                1,
                target_channels as usize,
                FixedSync::Input,
            ) {
                Ok(resampler) => Some(resampler),
                Err(err) => {
                    eprintln!(
                        "Warning: failed to create resampler {}->{}Hz: {}. Falling back to passthrough.",
                        source_rate, target_rate, err
                    );
                    None
                }
            }
        } else {
            None
        };

        Self {
            resampler,
            source_rate,
            input_buffer: VecDeque::with_capacity(chunk_size * 4),
            output_queue: VecDeque::with_capacity(chunk_size * 4),
            current_timestamp: 0,
            buffered_samples: 0,
            ready_queue: VecDeque::with_capacity(8),
        }
    }

    fn reset(&mut self) {
        self.input_buffer.clear();
        self.output_queue.clear();
        self.current_timestamp = 0;
        self.buffered_samples = 0;
        self.ready_queue.clear();
    }
}

/// Resampling processor that converts between different sample rates
pub struct ResampleProcessor {
    mic_state: StreamState,
    sys_state: StreamState,
    chunk_size: usize,
    target_rate: u32,
    target_channels: u16,
    source_type: AudioSourceType,
    last_processed_source: AudioSourceType,

    // Reusable buffer to avoid allocations in channel conversion
    mix_buffer: Vec<f32>,
}

impl ResampleProcessor {
    pub fn new(
        source_rate: u32,
        target_rate: u32,
        target_channels: u16,
        source_type: AudioSourceType,
    ) -> Self {
        let chunk_size = 1024;

        Self {
            mic_state: StreamState::new(source_rate, target_rate, target_channels, chunk_size),
            sys_state: StreamState::new(source_rate, target_rate, target_channels, chunk_size),
            chunk_size,
            target_rate,
            target_channels,
            source_type,
            last_processed_source: source_type,
            mix_buffer: Vec::with_capacity(chunk_size * 2),
        }
    }

    pub fn from_config(config: &AudioProcessingConfig, source_type: AudioSourceType) -> Self {
        Self::new(48000, config.sample_rate, config.channels, source_type)
    }

    fn state_mut(&mut self, source: AudioSourceType) -> &mut StreamState {
        match source {
            AudioSourceType::Microphone => &mut self.mic_state,
            AudioSourceType::System => &mut self.sys_state,
        }
    }

    fn convert_channels(&mut self, frame: &AudioFrame) -> Vec<f32> {
        if self.target_channels == 1 && frame.channels > 1 {
            // Multi-channel -> Mono downmix
            self.mix_buffer.clear();
            let ch_count = frame.channels as usize;
            for chunk in frame.samples.chunks_exact(ch_count) {
                self.mix_buffer.push(chunk.iter().sum::<f32>() / ch_count as f32);
            }
            self.mix_buffer.clone()
        } else if self.target_channels == 2 && frame.channels == 1 {
            // Mono -> Stereo upmix
            self.mix_buffer.clear();
            for &sample in &frame.samples {
                self.mix_buffer.push(sample);
                self.mix_buffer.push(sample);
            }
            self.mix_buffer.clone()
        } else {
            frame.samples.clone()
        }
    }

    fn process_resampling_state(
        state: &mut StreamState,
        chunk_size: usize,
        target_rate: u32,
        target_channels: u16,
        source: AudioSourceType,
    ) -> Vec<AudioFrame> {
        let mut results = Vec::new();
        let channels = target_channels as usize;

        if let Some(resampler) = &mut state.resampler {
            let needed = chunk_size * channels;
            let mut planar_data: Vec<Vec<f32>> = (0..channels)
                .map(|_| Vec::with_capacity(chunk_size))
                .collect();

            while state.input_buffer.len() >= needed {
                for channel_buf in &mut planar_data {
                    channel_buf.clear();
                }

                if channels == 1 {
                    for _ in 0..chunk_size {
                        let Some(sample) = state.input_buffer.pop_front() else {
                            return results;
                        };
                        planar_data[0].push(sample);
                    }
                } else {
                    for _ in 0..chunk_size {
                        for channel_buf in &mut planar_data {
                            let Some(sample) = state.input_buffer.pop_front() else {
                                return results;
                            };
                            channel_buf.push(sample);
                        }
                    }
                }

                let planar_input = PlanarBuffer {
                    data: &planar_data,
                    channels,
                    frames: chunk_size,
                };

                if let Ok(output) = resampler.process(&planar_input, 0, None) {
                    let mut samples = Vec::new();
                    if channels == 1 {
                        for i in 0..output.frames() {
                            if let Some(sample) = output.read_sample(0, i) {
                                samples.push(sample);
                            } else {
                                return results;
                            }
                        }
                    } else {
                        for i in 0..output.frames() {
                            for ch in 0..channels {
                                if let Some(sample) = output.read_sample(ch, i) {
                                    samples.push(sample);
                                } else {
                                    return results;
                                }
                            }
                        }
                    }

                    if !samples.is_empty() {
                        let frame_samples = (samples.len() / channels) as u64;
                        let frame_ts = state.current_timestamp
                            + (state.buffered_samples * 1_000_000_000 / target_rate as u64);
                        state.buffered_samples += frame_samples;

                        results.push(AudioFrame {
                            source,
                            samples,
                            sample_rate: target_rate,
                            channels: target_channels,
                            timestamp: frame_ts,
                        });
                    }
                }
            }
        } else if !state.output_queue.is_empty() {
            let samples: Vec<f32> = state.output_queue.drain(..).collect();
            let frame_samples = (samples.len() / channels) as u64;
            let frame_ts = state.current_timestamp
                + (state.buffered_samples * 1_000_000_000 / target_rate as u64);
            state.buffered_samples += frame_samples;

            results.push(AudioFrame {
                source,
                samples,
                sample_rate: if state.source_rate == target_rate {
                    target_rate
                } else {
                    state.source_rate
                },
                channels: target_channels,
                timestamp: frame_ts,
            });
        }

        results
    }

    /// Extract exactly 10ms frames at target rate
    pub fn pop_10ms_frame(&mut self) -> Option<AudioFrame> {
        let frame_size = (self.target_rate / 100) as usize;
        let channels = self.target_channels as usize;
        let needed_samples = frame_size * channels;
        let target_rate = self.target_rate;
        let target_channels = self.target_channels;
        let source = self.source_type;
        let state = self.state_mut(self.source_type);

        if state.output_queue.len() < needed_samples {
            return None;
        }

        let samples: Vec<f32> = state.output_queue.drain(..needed_samples).collect();
        let frame_ts = state.current_timestamp
            + (state.buffered_samples * 1_000_000_000 / target_rate as u64);
        state.buffered_samples += frame_size as u64;

        Some(AudioFrame {
            source,
            samples,
            sample_rate: target_rate,
            channels: target_channels,
            timestamp: frame_ts,
        })
    }

    /// Extract native 48kHz mono frames for AEC processing
    pub fn pop_native_frame(&mut self) -> Option<AudioFrame> {
        let frame_size = 480_usize; // 10ms at 48kHz
        let input_channels = self.target_channels as usize;
        let source = self.source_type;
        let state = self.state_mut(self.source_type);

        if input_channels == 2 {
            let needed_samples = frame_size * 2;
            if state.output_queue.len() < needed_samples {
                return None;
            }

            let stereo_samples: Vec<f32> = state.output_queue.drain(..needed_samples).collect();
            let mut mono_samples = Vec::with_capacity(frame_size);
            for chunk in stereo_samples.chunks_exact(2) {
                mono_samples.push((chunk[0] + chunk[1]) * 0.5);
            }

            let frame_ts = state.current_timestamp
                + (state.buffered_samples * 1_000_000_000 / 48_000_u64);
            state.buffered_samples += frame_size as u64;

            Some(AudioFrame {
                source,
                samples: mono_samples,
                sample_rate: 48_000,
                channels: 1,
                timestamp: frame_ts,
            })
        } else {
            if state.output_queue.len() < frame_size {
                return None;
            }

            let samples: Vec<f32> = state.output_queue.drain(..frame_size).collect();
            let frame_ts = state.current_timestamp
                + (state.buffered_samples * 1_000_000_000 / 48_000_u64);
            state.buffered_samples += frame_size as u64;

            Some(AudioFrame {
                source,
                samples,
                sample_rate: 48_000,
                channels: 1,
                timestamp: frame_ts,
            })
        }
    }
}

impl AudioProcessor for ResampleProcessor {
    fn process(&mut self, frame: AudioFrame) -> Result<Option<AudioFrame>> {
        let source = frame.source;
        self.last_processed_source = source;

        let samples_ref = self.convert_channels(&frame);
        let chunk_size = self.chunk_size;
        let target_rate = self.target_rate;
        let target_channels = self.target_channels;

        let state = self.state_mut(source);
        if state.input_buffer.is_empty() && state.output_queue.is_empty() {
            state.current_timestamp = frame.timestamp;
            state.buffered_samples = 0;
        }

        if state.resampler.is_some() {
            state.input_buffer.extend(&samples_ref);
        } else {
            state.output_queue.extend(&samples_ref);
        }

        let frames = Self::process_resampling_state(
            state,
            chunk_size,
            target_rate,
            target_channels,
            source,
        );
        state.ready_queue.extend(frames);

        Ok(state.ready_queue.pop_front())
    }

    fn drain_ready(&mut self) -> Result<Option<AudioFrame>> {
        let source = self.last_processed_source;
        let state = self.state_mut(source);
        Ok(state.ready_queue.pop_front())
    }

    fn flush(&mut self) -> Vec<AudioFrame> {
        let chunk_size = self.chunk_size;
        let target_rate = self.target_rate;
        let target_channels = self.target_channels;
        let mut frames = Vec::new();

        {
            let state = self.state_mut(AudioSourceType::System);
            frames.extend(state.ready_queue.drain(..));
            frames.extend(Self::process_resampling_state(
                state,
                chunk_size,
                target_rate,
                target_channels,
                AudioSourceType::System,
            ));
        }

        {
            let state = self.state_mut(AudioSourceType::Microphone);
            frames.extend(state.ready_queue.drain(..));
            frames.extend(Self::process_resampling_state(
                state,
                chunk_size,
                target_rate,
                target_channels,
                AudioSourceType::Microphone,
            ));
        }

        frames
    }

    fn reset(&mut self) {
        self.mic_state.reset();
        self.sys_state.reset();
        self.last_processed_source = self.source_type;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AudioProcessingConfig;

    fn frame(source: AudioSourceType, samples: Vec<f32>, channels: u16, ts: u64) -> AudioFrame {
        AudioFrame {
            source,
            samples,
            sample_rate: 48_000,
            channels,
            timestamp: ts,
        }
    }

    #[test]
    fn passthrough_when_sample_rate_matches() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::Microphone);
        let input = frame(AudioSourceType::Microphone, vec![0.1, 0.2, 0.3], 1, 123);
        let out = p.process(input).unwrap().unwrap();

        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 1);
        assert_eq!(out.timestamp, 123);
        assert_eq!(out.samples, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn downmixes_stereo_to_mono() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::Microphone);
        let input = frame(AudioSourceType::Microphone, vec![1.0, 0.0, 0.5, -0.5], 2, 77);
        let out = p.process(input).unwrap().unwrap();

        assert_eq!(out.samples, vec![0.5, 0.0]);
        assert_eq!(out.channels, 1);
    }

    #[test]
    fn upmixes_mono_to_stereo() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 2, AudioSourceType::Microphone);
        let input = frame(AudioSourceType::Microphone, vec![0.25, -0.25], 1, 33);
        let out = p.process(input).unwrap().unwrap();

        assert_eq!(out.samples, vec![0.25, 0.25, -0.25, -0.25]);
        assert_eq!(out.channels, 2);
    }

    #[test]
    fn keeps_sources_isolated() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::System);
        let sys = p.process(frame(AudioSourceType::System, vec![1.0; 4], 1, 1)).unwrap().unwrap();
        let mic = p.process(frame(AudioSourceType::Microphone, vec![2.0; 4], 1, 2)).unwrap().unwrap();

        assert_eq!(sys.source, AudioSourceType::System);
        assert_eq!(mic.source, AudioSourceType::Microphone);
        assert_eq!(sys.samples, vec![1.0; 4]);
        assert_eq!(mic.samples, vec![2.0; 4]);
    }

    #[test]
    fn pop_native_frame_converts_stereo_queue_to_mono() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 2, AudioSourceType::System);
        p.sys_state.output_queue.extend(std::iter::repeat_n(1.0_f32, 960));
        let f = p.pop_native_frame().unwrap();

        assert_eq!(f.source, AudioSourceType::System);
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.channels, 1);
        assert_eq!(f.samples.len(), 480);
        assert!(f.samples.iter().all(|&s| (s - 1.0).abs() < f32::EPSILON));
    }

    #[test]
    fn from_config_applies_target_format() {
        let cfg = AudioProcessingConfig {
            sample_rate: 16_000,
            channels: 2,
            enable_aec: false,
            enable_ns: false,
            sample_format: "f32".to_string(),
            aec_stream_delay_ms: 0,
            aec_auto_delay_tuning: false,
            aec_max_delay_ms: 140,
        };
        let p = ResampleProcessor::from_config(&cfg, AudioSourceType::Microphone);
        assert_eq!(p.target_rate, 16_000);
        assert_eq!(p.target_channels, 2);
    }

    #[test]
    fn pop_10ms_frame_uses_selected_source_queue() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::Microphone);
        p.mic_state.output_queue.extend(std::iter::repeat_n(0.5_f32, 480));
        let f = p.pop_10ms_frame().unwrap();
        assert_eq!(f.source, AudioSourceType::Microphone);
        assert_eq!(f.samples.len(), 480);
    }

    #[test]
    fn flush_drains_ready_queues_for_both_sources() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::Microphone);
        p.mic_state.ready_queue.push_back(frame(AudioSourceType::Microphone, vec![1.0], 1, 0));
        p.sys_state.ready_queue.push_back(frame(AudioSourceType::System, vec![2.0], 1, 0));
        let mut out = p.flush();
        out.sort_by_key(|f| match f.source { AudioSourceType::System => 0, AudioSourceType::Microphone => 1 });
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].source, AudioSourceType::System);
        assert_eq!(out[1].source, AudioSourceType::Microphone);
    }

    #[test]
    fn reset_clears_internal_buffers() {
        let mut p = ResampleProcessor::new(48_000, 48_000, 1, AudioSourceType::System);
        p.mic_state.output_queue.extend([1.0, 2.0, 3.0]);
        p.sys_state.output_queue.extend([4.0, 5.0]);
        p.last_processed_source = AudioSourceType::Microphone;
        p.reset();
        assert!(p.mic_state.output_queue.is_empty());
        assert!(p.sys_state.output_queue.is_empty());
        assert_eq!(p.last_processed_source, AudioSourceType::System);
    }
}
