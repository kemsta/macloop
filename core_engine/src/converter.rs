use crate::format::{SampleFormat, StreamFormat};
use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    calculate_cutoff, Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

#[derive(Debug)]
pub enum InputConversionError {
    InvalidInputLen {
        input_len: usize,
        channels: u16,
    },
    UnsupportedChannelConversion {
        input_channels: u16,
        output_channels: u16,
    },
    UnsupportedSampleFormat {
        input: SampleFormat,
        output: SampleFormat,
    },
    ResamplerInit(String),
    ResamplerProcess(String),
}

impl std::fmt::Display for InputConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInputLen {
                input_len,
                channels,
            } => write!(
                f,
                "input length {input_len} is not divisible by channels {channels}"
            ),
            Self::UnsupportedChannelConversion {
                input_channels,
                output_channels,
            } => write!(
                f,
                "unsupported channel conversion: {input_channels} -> {output_channels}"
            ),
            Self::UnsupportedSampleFormat { input, output } => {
                write!(
                    f,
                    "unsupported sample format conversion: {:?} -> {:?}",
                    input, output
                )
            }
            Self::ResamplerInit(msg) => write!(f, "resampler initialization failed: {msg}"),
            Self::ResamplerProcess(msg) => write!(f, "resampler process failed: {msg}"),
        }
    }
}

impl std::error::Error for InputConversionError {}

pub trait InputConverter: Send {
    fn input_format(&self) -> StreamFormat;
    fn output_format(&self) -> StreamFormat;
    fn convert(&mut self, input: &[f32], output: &mut Vec<f32>)
        -> Result<(), InputConversionError>;
}

pub struct MasterFormatConverter {
    input_format: StreamFormat,
    output_format: StreamFormat,
    resampler: Option<Async<f32>>,
    channels_buffer: Vec<f32>,
    pending_interleaved: Vec<f32>,
    deinterleaved_in: Vec<Vec<f32>>,
    deinterleaved_out: Vec<Vec<f32>>,
    output_delay_frames_remaining: usize,
    total_input_frames: u64,
    total_output_frames: u64,
}

impl MasterFormatConverter {
    pub fn new(
        input_format: StreamFormat,
        output_format: StreamFormat,
    ) -> Result<Self, InputConversionError> {
        if input_format.sample_format != SampleFormat::F32
            || output_format.sample_format != SampleFormat::F32
        {
            return Err(InputConversionError::UnsupportedSampleFormat {
                input: input_format.sample_format,
                output: output_format.sample_format,
            });
        }

        let resampler = if input_format.sample_rate != output_format.sample_rate {
            let ratio = output_format.sample_rate as f64 / input_format.sample_rate as f64;
            let params = SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: calculate_cutoff::<f32>(64, WindowFunction::BlackmanHarris2),
                interpolation: SincInterpolationType::Cubic,
                oversampling_factor: 32,
                window: WindowFunction::BlackmanHarris2,
            };

            Some(
                Async::<f32>::new_sinc(
                    ratio,
                    1.5,
                    &params,
                    1024,
                    output_format.channels as usize,
                    FixedAsync::Input,
                )
                .map_err(|e| InputConversionError::ResamplerInit(e.to_string()))?,
            )
        } else {
            None
        };

        let output_delay_frames_remaining =
            resampler.as_ref().map(Resampler::output_delay).unwrap_or(0);

        Ok(Self {
            input_format,
            output_format,
            resampler,
            channels_buffer: Vec::new(),
            pending_interleaved: Vec::new(),
            deinterleaved_in: Vec::new(),
            deinterleaved_out: Vec::new(),
            output_delay_frames_remaining,
            total_input_frames: 0,
            total_output_frames: 0,
        })
    }

    pub fn finish(&mut self, output: &mut Vec<f32>) -> Result<(), InputConversionError> {
        output.clear();
        if self.resampler.is_none() {
            return Ok(());
        }

        let target_output_frames = self.expected_output_frames();
        let channels = self.output_format.channels as usize;
        while self.total_output_frames < target_output_frames {
            let input_frames_next = self
                .resampler
                .as_ref()
                .expect("resampler presence checked above")
                .input_frames_next();
            let pending_frames = self.pending_interleaved.len() / channels;
            let partial_frames = pending_frames.min(input_frames_next);
            let partial_samples = partial_frames * channels;
            Self::deinterleave_padded_into(
                &mut self.deinterleaved_in,
                &self.pending_interleaved[..partial_samples],
                channels,
                input_frames_next,
            );

            let (used_in, used_out) = {
                let resampler = self
                    .resampler
                    .as_mut()
                    .expect("resampler presence checked above");
                let adapter_in =
                    SequentialSliceOfVecs::new(&self.deinterleaved_in, channels, input_frames_next)
                        .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?;

                let output_frames_cap = resampler.output_frames_max().max(1);
                Self::ensure_output_storage(
                    &mut self.deinterleaved_out,
                    channels,
                    output_frames_cap,
                );
                let mut adapter_out = SequentialSliceOfVecs::new_mut(
                    &mut self.deinterleaved_out,
                    channels,
                    output_frames_cap,
                )
                .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?;

                resampler
                    .process_into_buffer(
                        &adapter_in,
                        &mut adapter_out,
                        Some(&Indexing {
                            input_offset: 0,
                            output_offset: 0,
                            partial_len: Some(partial_frames),
                            active_channels_mask: None,
                        }),
                    )
                    .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?
            };

            if partial_frames > 0 {
                let consumed_samples = used_in.min(partial_frames) * channels;
                self.pending_interleaved.drain(..consumed_samples);
            }
            if used_out == 0 {
                return Err(InputConversionError::ResamplerProcess(
                    "resampler produced no output while finishing".to_string(),
                ));
            }

            self.append_trimmed_resampler_output(used_out, target_output_frames, output);
        }

        self.pending_interleaved.clear();
        Ok(())
    }

    fn expected_output_frames(&self) -> u64 {
        let numerator = self.total_input_frames as u128 * self.output_format.sample_rate as u128;
        let denominator = self.input_format.sample_rate as u128;
        let frames = numerator.div_ceil(denominator);
        frames.min(u64::MAX as u128) as u64
    }

    fn convert_channels(
        input: &[f32],
        input_channels: u16,
        output_channels: u16,
        output: &mut Vec<f32>,
    ) -> Result<(), InputConversionError> {
        let in_channels = input_channels as usize;
        let out_channels = output_channels as usize;

        if in_channels == 0 || input.len() % in_channels != 0 {
            return Err(InputConversionError::InvalidInputLen {
                input_len: input.len(),
                channels: input_channels,
            });
        }

        if in_channels == out_channels {
            output.clear();
            output.extend_from_slice(input);
            return Ok(());
        }

        if in_channels == 1 && out_channels == 2 {
            output.clear();
            output.reserve(input.len() * 2);
            for &sample in input {
                output.push(sample);
                output.push(sample);
            }
            return Ok(());
        }

        if in_channels == 2 && out_channels == 1 {
            output.clear();
            output.reserve(input.len() / 2);
            for frame in input.chunks_exact(2) {
                output.push((frame[0] + frame[1]) * 0.5);
            }
            return Ok(());
        }

        Err(InputConversionError::UnsupportedChannelConversion {
            input_channels,
            output_channels,
        })
    }

    fn ensure_channels_storage(storage: &mut Vec<Vec<f32>>, channels: usize, frames: usize) {
        storage.clear();
        storage.resize_with(channels, Vec::new);
        for channel in storage.iter_mut() {
            channel.clear();
            channel.reserve(frames);
        }
    }

    fn deinterleave_into(storage: &mut Vec<Vec<f32>>, input: &[f32], channels: usize) {
        let frames = input.len() / channels;
        Self::ensure_channels_storage(storage, channels, frames);
        for frame in 0..frames {
            let base = frame * channels;
            for ch in 0..channels {
                storage[ch].push(input[base + ch]);
            }
        }
    }

    fn deinterleave_padded_into(
        storage: &mut Vec<Vec<f32>>,
        input: &[f32],
        channels: usize,
        padded_frames: usize,
    ) {
        Self::ensure_channels_storage(storage, channels, padded_frames);
        for channel in storage.iter_mut() {
            channel.resize(padded_frames, 0.0);
        }
        for (frame_index, frame) in input.chunks_exact(channels).enumerate() {
            for (channel_index, sample) in frame.iter().enumerate() {
                storage[channel_index][frame_index] = *sample;
            }
        }
    }

    fn ensure_output_storage(storage: &mut Vec<Vec<f32>>, channels: usize, frames_cap: usize) {
        if storage.len() != channels {
            storage.clear();
            storage.resize_with(channels, Vec::new);
        }
        for channel in storage.iter_mut() {
            if channel.len() != frames_cap {
                channel.clear();
                channel.resize(frames_cap, 0.0);
            }
        }
    }

    fn interleave_range_append_to(
        channels_data: &[Vec<f32>],
        first_frame: usize,
        frames: usize,
        out: &mut Vec<f32>,
    ) {
        let channels = channels_data.len();
        out.reserve(frames * channels);
        for frame in first_frame..first_frame + frames {
            for channel in channels_data.iter().take(channels) {
                out.push(channel[frame]);
            }
        }
    }

    fn append_trimmed_resampler_output(
        &mut self,
        produced_frames: usize,
        target_output_frames: u64,
        output: &mut Vec<f32>,
    ) {
        let delay_frames = self.output_delay_frames_remaining.min(produced_frames);
        self.output_delay_frames_remaining -= delay_frames;

        let available_frames = produced_frames - delay_frames;
        let remaining_target_frames = target_output_frames
            .saturating_sub(self.total_output_frames)
            .min(usize::MAX as u64) as usize;
        let emitted_frames = available_frames.min(remaining_target_frames);
        Self::interleave_range_append_to(
            &self.deinterleaved_out,
            delay_frames,
            emitted_frames,
            output,
        );
        self.total_output_frames = self
            .total_output_frames
            .saturating_add(emitted_frames as u64);
    }
}

impl InputConverter for MasterFormatConverter {
    fn input_format(&self) -> StreamFormat {
        self.input_format
    }

    fn output_format(&self) -> StreamFormat {
        self.output_format
    }

    fn convert(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), InputConversionError> {
        Self::convert_channels(
            input,
            self.input_format.channels,
            self.output_format.channels,
            &mut self.channels_buffer,
        )?;
        let converted_input_frames =
            self.channels_buffer.len() / self.output_format.channels as usize;
        self.total_input_frames = self
            .total_input_frames
            .saturating_add(converted_input_frames as u64);

        if self.resampler.is_none() {
            output.clear();
            output.extend_from_slice(&self.channels_buffer);
            self.total_output_frames = self
                .total_output_frames
                .saturating_add(converted_input_frames as u64);
            return Ok(());
        }

        output.clear();
        let channels = self.output_format.channels as usize;
        self.pending_interleaved
            .extend_from_slice(&self.channels_buffer);
        let target_output_frames = self.expected_output_frames();

        let mut consumed_frames = 0usize;
        loop {
            let pending_frames = self.pending_interleaved.len() / channels;
            let input_frames_next = self
                .resampler
                .as_ref()
                .expect("resampler presence checked above")
                .input_frames_next();
            if pending_frames.saturating_sub(consumed_frames) < input_frames_next {
                break;
            }

            let start = consumed_frames * channels;
            let end = start + input_frames_next * channels;
            Self::deinterleave_into(
                &mut self.deinterleaved_in,
                &self.pending_interleaved[start..end],
                channels,
            );

            let used_out = {
                let resampler = self
                    .resampler
                    .as_mut()
                    .expect("resampler presence checked above");
                let adapter_in =
                    SequentialSliceOfVecs::new(&self.deinterleaved_in, channels, input_frames_next)
                        .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?;

                let output_frames_cap = resampler.output_frames_max().max(1);
                Self::ensure_output_storage(
                    &mut self.deinterleaved_out,
                    channels,
                    output_frames_cap,
                );
                let mut adapter_out = SequentialSliceOfVecs::new_mut(
                    &mut self.deinterleaved_out,
                    channels,
                    output_frames_cap,
                )
                .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?;

                let (_used_in, used_out) = resampler
                    .process_into_buffer(
                        &adapter_in,
                        &mut adapter_out,
                        Some(&Indexing {
                            input_offset: 0,
                            output_offset: 0,
                            partial_len: None,
                            active_channels_mask: None,
                        }),
                    )
                    .map_err(|e| InputConversionError::ResamplerProcess(e.to_string()))?;
                used_out
            };

            self.append_trimmed_resampler_output(used_out, target_output_frames, output);
            consumed_frames += input_frames_next;
        }

        if consumed_frames > 0 {
            let consumed_samples = consumed_frames * channels;
            self.pending_interleaved.drain(..consumed_samples);
        }
        Ok(())
    }
}

pub fn convert_f32_to_i16(input: &[f32], output: &mut Vec<i16>) {
    output.clear();
    output.reserve(input.len());
    for &sample in input {
        let clamped = sample.clamp(-1.0, 1.0);
        let scaled = (clamped * i16::MAX as f32).round();
        output.push(scaled as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{SampleFormat, MASTER_FORMAT};

    #[test]
    fn convert_channels_rejects_invalid_input_len() {
        let mut out = Vec::new();
        let err = MasterFormatConverter::convert_channels(&[0.1, 0.2, 0.3], 2, 1, &mut out)
            .expect_err("invalid input length");

        assert!(matches!(
            err,
            InputConversionError::InvalidInputLen {
                input_len: 3,
                channels: 2
            }
        ));
    }

    #[test]
    fn convert_channels_rejects_unsupported_channel_conversion() {
        let mut out = Vec::new();
        let err = MasterFormatConverter::convert_channels(
            &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
            3,
            2,
            &mut out,
        )
        .expect_err("unsupported conversion");

        assert!(matches!(
            err,
            InputConversionError::UnsupportedChannelConversion {
                input_channels: 3,
                output_channels: 2
            }
        ));
    }

    #[test]
    fn converter_reports_input_and_output_formats() {
        let input = StreamFormat::new(48_000, 1);
        let output = StreamFormat::new(48_000, 2);
        let converter = MasterFormatConverter::new(input, output).expect("converter");

        assert_eq!(converter.input_format(), input);
        assert_eq!(converter.output_format(), output);
    }

    #[test]
    fn mono_to_stereo_without_resample() {
        let mut converter = MasterFormatConverter::new(StreamFormat::new(48_000, 1), MASTER_FORMAT)
            .expect("converter");

        let input = vec![0.1_f32, -0.2, 0.3];
        let mut out = Vec::new();
        converter.convert(&input, &mut out).expect("convert");

        assert_eq!(out, vec![0.1, 0.1, -0.2, -0.2, 0.3, 0.3]);
    }

    #[test]
    fn stereo_to_mono_without_resample() {
        let mut converter =
            MasterFormatConverter::new(StreamFormat::new(48_000, 2), StreamFormat::new(48_000, 1))
                .expect("converter");

        let input = vec![0.2_f32, 0.6, -0.4, 0.2];
        let mut out = Vec::new();
        converter.convert(&input, &mut out).expect("convert");

        assert_eq!(out, vec![0.4, -0.1]);
    }

    #[test]
    fn resamples_to_master_rate() {
        let mut converter = MasterFormatConverter::new(StreamFormat::new(16_000, 1), MASTER_FORMAT)
            .expect("converter");

        let input = vec![0.5_f32; 320];
        let mut out = Vec::new();
        let mut produced = 0usize;
        for _ in 0..6 {
            converter.convert(&input, &mut out).expect("convert");
            produced += out.len();
        }

        assert!(produced > 0);
        assert_eq!(produced % MASTER_FORMAT.channels as usize, 0);
    }

    fn convert_and_finish(converter: &mut MasterFormatConverter, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        converter.convert(input, &mut output).expect("convert");
        let mut all_output = output.clone();
        converter.finish(&mut output).expect("finish");
        all_output.extend_from_slice(&output);
        all_output
    }

    fn peak_index(samples: &[f32]) -> usize {
        samples
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
            .map(|(index, _)| index)
            .expect("non-empty samples")
    }

    #[test]
    fn finish_preserves_resampled_duration_for_partial_input() {
        let mut converter = MasterFormatConverter::new(
            MASTER_FORMAT,
            StreamFormat::with_sample_format(16_000, 1, SampleFormat::F32),
        )
        .expect("converter");
        let input = vec![0.25_f32; 4_800 * MASTER_FORMAT.channels as usize];
        let mut output = Vec::new();

        converter.convert(&input, &mut output).expect("convert");
        let mut all_output = output.clone();
        converter.finish(&mut output).expect("finish");
        all_output.extend_from_slice(&output);

        assert_eq!(all_output.len(), 1_600);
        assert!(all_output[200..1_400]
            .iter()
            .all(|sample| (*sample - 0.25).abs() < 1e-3));

        converter.finish(&mut output).expect("finish again");
        assert!(output.is_empty());
    }

    #[test]
    fn resampler_discards_startup_delay_for_beginning_impulse() {
        let mut converter = MasterFormatConverter::new(
            MASTER_FORMAT,
            StreamFormat::with_sample_format(16_000, 1, SampleFormat::F32),
        )
        .expect("converter");
        let mut input = vec![0.0_f32; 4_800 * MASTER_FORMAT.channels as usize];
        input[..MASTER_FORMAT.channels as usize].fill(1.0);

        let output = convert_and_finish(&mut converter, &input);

        assert_eq!(output.len(), 1_600);
        assert!(peak_index(&output) <= 1, "beginning impulse was delayed");
        assert!(output[..4].iter().any(|sample| sample.abs() > 0.1));
    }

    #[test]
    fn finish_preserves_end_impulse_in_zero_tail() {
        let mut converter = MasterFormatConverter::new(
            MASTER_FORMAT,
            StreamFormat::with_sample_format(16_000, 1, SampleFormat::F32),
        )
        .expect("converter");
        let mut input = vec![0.0_f32; 4_800 * MASTER_FORMAT.channels as usize];
        let last_frame = input.len() - MASTER_FORMAT.channels as usize;
        input[last_frame..].fill(1.0);

        let output = convert_and_finish(&mut converter, &input);

        assert_eq!(output.len(), 1_600);
        assert!(
            peak_index(&output) >= output.len() - 2,
            "end impulse was truncated"
        );
        assert!(output[output.len() - 4..]
            .iter()
            .any(|sample| sample.abs() > 0.1));
    }

    #[test]
    fn finish_emits_short_resampled_input() {
        let mut converter = MasterFormatConverter::new(
            MASTER_FORMAT,
            StreamFormat::with_sample_format(16_000, 1, SampleFormat::F32),
        )
        .expect("converter");
        let input = vec![0.25_f32; 100 * MASTER_FORMAT.channels as usize];
        let mut output = Vec::new();

        converter.convert(&input, &mut output).expect("convert");
        assert!(output.is_empty());
        converter.finish(&mut output).expect("finish");

        assert_eq!(output.len(), 34);
    }

    #[test]
    fn rejects_non_f32_formats_for_now() {
        match MasterFormatConverter::new(
            StreamFormat::with_sample_format(48_000, 1, SampleFormat::I16),
            MASTER_FORMAT,
        ) {
            Err(InputConversionError::UnsupportedSampleFormat { .. }) => {}
            _ => panic!("i16 currently unsupported"),
        }
    }

    #[test]
    fn quantizes_f32_to_i16() {
        let mut out = Vec::new();
        convert_f32_to_i16(&[-1.5, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5], &mut out);

        assert_eq!(
            out,
            vec![
                i16::MIN + 1,
                i16::MIN + 1,
                -16384,
                0,
                16384,
                i16::MAX,
                i16::MAX
            ]
        );
    }
}
