use std::collections::VecDeque;
use crate::messages::{AudioFrame, AudioSourceType};
use super::AudioProcessor;
use anyhow::Result;

/// Frame Quantizer - converts variable-size frames to fixed quantum size
/// Essential for WebRTC which expects exactly 480 samples (10ms at 48kHz)
pub struct FrameQuantizer {
    mic_state: QuantizerState,
    sys_state: QuantizerState,
    ready_queue: VecDeque<AudioFrame>,
    quantum_size: usize, // Target frame size in samples
    expected_sample_rate: u32,
    expected_channels: u16,
}

struct QuantizerState {
    sample_buffer: VecDeque<f32>,
    current_timestamp: u64,
    samples_processed: u64,
}

impl QuantizerState {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            sample_buffer: VecDeque::with_capacity(capacity),
            current_timestamp: 0,
            samples_processed: 0,
        }
    }
}

impl FrameQuantizer {
    /// Create quantizer for WebRTC (480 samples = 10ms at 48kHz mono)
    pub fn for_webrtc() -> Self {
        Self {
            mic_state: QuantizerState::with_capacity(2048),
            sys_state: QuantizerState::with_capacity(2048),
            ready_queue: VecDeque::with_capacity(8),
            quantum_size: 480, // 10ms at 48kHz
            expected_sample_rate: 48000,
            expected_channels: 1,
        }
    }
    
    /// Create quantizer with custom quantum size
    pub fn with_quantum_size(quantum_size: usize, sample_rate: u32, channels: u16) -> Self {
        Self {
            mic_state: QuantizerState::with_capacity(quantum_size * 4),
            sys_state: QuantizerState::with_capacity(quantum_size * 4),
            ready_queue: VecDeque::with_capacity(8),
            quantum_size,
            expected_sample_rate: sample_rate,
            expected_channels: channels,
        }
    }

    fn source_state_mut(&mut self, source: AudioSourceType) -> &mut QuantizerState {
        match source {
            AudioSourceType::Microphone => &mut self.mic_state,
            AudioSourceType::System => &mut self.sys_state,
        }
    }

    fn extract_quantum_from_state(
        source: AudioSourceType,
        state: &mut QuantizerState,
        quantum_size: usize,
        expected_sample_rate: u32,
        expected_channels: u16,
    ) -> Option<AudioFrame> {
        if state.sample_buffer.len() < quantum_size {
            return None;
        }

        let samples: Vec<f32> = state.sample_buffer.drain(..quantum_size).collect();
        let quantum_timestamp = state.current_timestamp
            + (state.samples_processed * 1_000_000_000) / expected_sample_rate as u64;
        state.samples_processed += quantum_size as u64;

        Some(AudioFrame {
            source,
            samples,
            sample_rate: expected_sample_rate,
            channels: expected_channels,
            timestamp: quantum_timestamp,
        })
    }

    fn enqueue_ready_quanta(&mut self, source: AudioSourceType, frame_timestamp: u64, frame_samples: &[f32]) {
        let quantum_size = self.quantum_size;
        let expected_sample_rate = self.expected_sample_rate;
        let expected_channels = self.expected_channels;
        let mut local_ready = VecDeque::new();
        {
            let state = self.source_state_mut(source);
            if state.sample_buffer.is_empty() && state.samples_processed == 0 {
                state.current_timestamp = frame_timestamp;
            }
            state.sample_buffer.extend(frame_samples.iter().copied());
        }

        loop {
            let next = {
                let state = self.source_state_mut(source);
                Self::extract_quantum_from_state(
                    source,
                    state,
                    quantum_size,
                    expected_sample_rate,
                    expected_channels,
                )
            };
            match next {
                Some(frame) => local_ready.push_back(frame),
                None => break,
            }
        }

        self.ready_queue.extend(local_ready);
    }

    fn flush_source(&mut self, source: AudioSourceType) -> Vec<AudioFrame> {
        let quantum_size = self.quantum_size;
        let expected_sample_rate = self.expected_sample_rate;
        let expected_channels = self.expected_channels;
        let mut results = Vec::new();

        loop {
            let next = {
                let state = self.source_state_mut(source);
                Self::extract_quantum_from_state(
                    source,
                    state,
                    quantum_size,
                    expected_sample_rate,
                    expected_channels,
                )
            };
            match next {
                Some(frame) => results.push(frame),
                None => break,
            }
        }

        let remaining = {
            let state = self.source_state_mut(source);
            if state.sample_buffer.is_empty() {
                None
            } else {
                let mut samples: Vec<f32> = state.sample_buffer.drain(..).collect();
                samples.resize(quantum_size, 0.0);
                let timestamp = state.current_timestamp
                    + (state.samples_processed * 1_000_000_000) / expected_sample_rate as u64;
                Some(AudioFrame {
                    source,
                    samples,
                    sample_rate: expected_sample_rate,
                    channels: expected_channels,
                    timestamp,
                })
            }
        };

        if let Some(frame) = remaining {
            results.push(frame);
        }

        results
    }
}

impl AudioProcessor for FrameQuantizer {
    fn process(&mut self, frame: AudioFrame) -> Result<Option<AudioFrame>> {
        // Validate input format
        if frame.sample_rate != self.expected_sample_rate {
            return Ok(None);
        }

        if frame.channels != self.expected_channels {
            return Ok(None);
        }

        self.enqueue_ready_quanta(frame.source, frame.timestamp, &frame.samples);
        Ok(self.ready_queue.pop_front())
    }

    fn drain_ready(&mut self) -> Result<Option<AudioFrame>> {
        Ok(self.ready_queue.pop_front())
    }

    fn flush(&mut self) -> Vec<AudioFrame> {
        let mut results = Vec::new();

        while let Some(frame) = self.ready_queue.pop_front() {
            results.push(frame);
        }

        results.extend(self.flush_source(AudioSourceType::System));
        results.extend(self.flush_source(AudioSourceType::Microphone));

        results
    }

    fn reset(&mut self) {
        self.mic_state.sample_buffer.clear();
        self.mic_state.current_timestamp = 0;
        self.mic_state.samples_processed = 0;
        self.sys_state.sample_buffer.clear();
        self.sys_state.current_timestamp = 0;
        self.sys_state.samples_processed = 0;
        self.ready_queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(source: AudioSourceType, samples: usize, ts: u64, rate: u32, ch: u16) -> AudioFrame {
        AudioFrame {
            source,
            samples: vec![1.0; samples],
            sample_rate: rate,
            channels: ch,
            timestamp: ts,
        }
    }

    #[test]
    fn rejects_unexpected_format() {
        let mut q = FrameQuantizer::for_webrtc();
        let bad_rate = q.process(frame(AudioSourceType::Microphone, 480, 0, 16_000, 1)).unwrap();
        let bad_ch = q.process(frame(AudioSourceType::Microphone, 480, 0, 48_000, 2)).unwrap();

        assert!(bad_rate.is_none());
        assert!(bad_ch.is_none());
    }

    #[test]
    fn emits_quantized_frames_and_drain_ready() {
        let mut q = FrameQuantizer::for_webrtc();
        let first = q
            .process(frame(AudioSourceType::Microphone, 960, 1_000_000, 48_000, 1))
            .unwrap()
            .unwrap();
        let second = q.drain_ready().unwrap().unwrap();
        let none = q.drain_ready().unwrap();

        assert_eq!(first.samples.len(), 480);
        assert_eq!(second.samples.len(), 480);
        assert_eq!(second.timestamp.saturating_sub(first.timestamp), 10_000_000);
        assert!(none.is_none());
    }

    #[test]
    fn keeps_mic_and_system_streams_independent() {
        let mut q = FrameQuantizer::for_webrtc();
        let mic = q.process(frame(AudioSourceType::Microphone, 480, 10_000, 48_000, 1)).unwrap();
        let sys = q.process(frame(AudioSourceType::System, 480, 20_000, 48_000, 1)).unwrap();

        assert_eq!(mic.unwrap().source, AudioSourceType::Microphone);
        assert_eq!(sys.unwrap().source, AudioSourceType::System);
    }

    #[test]
    fn flush_outputs_padded_tail() {
        let mut q = FrameQuantizer::with_quantum_size(8, 48_000, 1);
        let _ = q.process(frame(AudioSourceType::Microphone, 3, 0, 48_000, 1)).unwrap();
        let flushed = q.flush();

        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].samples.len(), 8);
        assert_eq!(flushed[0].samples[0], 1.0);
        assert_eq!(flushed[0].samples[7], 0.0);
    }
}
