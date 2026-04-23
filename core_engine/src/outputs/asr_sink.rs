use crate::converter::{
    convert_f32_to_i16, InputConversionError, InputConverter, MasterFormatConverter,
};
use crate::engine::RouteConsumer;
use crate::format::{SampleFormat, StreamFormat, MASTER_FORMAT};
use crate::metrics::LatencyHistogram;
use ringbuf::traits::{Consumer, Observer};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub type AsrInputId = String;

pub struct AsrSinkInput {
    pub input_id: AsrInputId,
    pub consumer: RouteConsumer,
}

#[derive(Debug, Clone, Copy)]
pub struct AsrSinkConfig {
    pub format: StreamFormat,
    pub chunk_frames: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum AsrSampleSlice<'a> {
    F32(&'a [f32]),
    I16(&'a [i16]),
}

#[derive(Debug, Clone, Copy)]
pub struct AsrChunkView<'a> {
    pub input_id: &'a str,
    pub frames: usize,
    pub samples: AsrSampleSlice<'a>,
}

pub trait AsrSinkCallback: Send {
    fn on_chunk(&mut self, chunk: AsrChunkView<'_>);
}

impl<F> AsrSinkCallback for F
where
    F: for<'a> FnMut(AsrChunkView<'a>) + Send,
{
    fn on_chunk(&mut self, chunk: AsrChunkView<'_>) {
        self(chunk);
    }
}

#[derive(Debug)]
pub enum AsrSinkError {
    NoInputs,
    InvalidChunkFrames,
    UnsupportedOutputChannels(u16),
    Converter(InputConversionError),
    ThreadPanic,
    AlreadyStopped,
}

impl std::fmt::Display for AsrSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInputs => write!(f, "asr sink requires at least one input"),
            Self::InvalidChunkFrames => write!(f, "chunk_frames must be greater than zero"),
            Self::UnsupportedOutputChannels(channels) => {
                write!(f, "unsupported output channels for asr sink: {channels}")
            }
            Self::Converter(err) => write!(f, "converter error: {err}"),
            Self::ThreadPanic => write!(f, "asr sink thread panicked"),
            Self::AlreadyStopped => write!(f, "asr sink already stopped"),
        }
    }
}

impl std::error::Error for AsrSinkError {}

impl From<InputConversionError> for AsrSinkError {
    fn from(value: InputConversionError) -> Self {
        Self::Converter(value)
    }
}

pub struct AsrInputMetrics {
    chunks_emitted: std::sync::atomic::AtomicU64,
    frames_emitted: std::sync::atomic::AtomicU64,
    pending_frames: std::sync::atomic::AtomicU32,
    poll: LatencyHistogram,
    callback: LatencyHistogram,
}

impl Default for AsrInputMetrics {
    fn default() -> Self {
        Self {
            chunks_emitted: std::sync::atomic::AtomicU64::new(0),
            frames_emitted: std::sync::atomic::AtomicU64::new(0),
            pending_frames: std::sync::atomic::AtomicU32::new(0),
            poll: LatencyHistogram::default(),
            callback: LatencyHistogram::default(),
        }
    }
}

impl AsrInputMetrics {
    fn snapshot(&self) -> AsrInputMetricsSnapshot {
        AsrInputMetricsSnapshot {
            chunks_emitted: self.chunks_emitted.load(Ordering::Relaxed),
            frames_emitted: self.frames_emitted.load(Ordering::Relaxed),
            pending_frames: self.pending_frames.load(Ordering::Relaxed),
            poll: self.poll.snapshot(),
            callback: self.callback.snapshot(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AsrInputMetricsSnapshot {
    pub chunks_emitted: u64,
    pub frames_emitted: u64,
    pub pending_frames: u32,
    pub poll: crate::metrics::LatencyHistogramSnapshot,
    pub callback: crate::metrics::LatencyHistogramSnapshot,
}

pub type AsrSinkMetricsSnapshot = HashMap<AsrInputId, AsrInputMetricsSnapshot>;

struct InputState {
    input_id: AsrInputId,
    consumer: RouteConsumer,
    target_format: StreamFormat,
    chunk_frames: usize,
    converter: MasterFormatConverter,
    drained_master: Vec<f32>,
    converted_output: Vec<f32>,
    pending_output: Vec<f32>,
    pending_offset: usize,
    quantized_output: Vec<i16>,
    metrics: Arc<AsrInputMetrics>,
}

impl InputState {
    fn new(
        input: AsrSinkInput,
        config: AsrSinkConfig,
        metrics: Arc<AsrInputMetrics>,
    ) -> Result<Self, AsrSinkError> {
        if config.chunk_frames == 0 {
            return Err(AsrSinkError::InvalidChunkFrames);
        }

        if !(1..=2).contains(&config.format.channels) {
            return Err(AsrSinkError::UnsupportedOutputChannels(
                config.format.channels,
            ));
        }

        let f32_format = StreamFormat::with_sample_format(
            config.format.sample_rate,
            config.format.channels,
            SampleFormat::F32,
        );

        Ok(Self {
            input_id: input.input_id,
            consumer: input.consumer,
            target_format: config.format,
            chunk_frames: config.chunk_frames,
            converter: MasterFormatConverter::new(MASTER_FORMAT, f32_format)?,
            drained_master: Vec::new(),
            converted_output: Vec::new(),
            pending_output: Vec::new(),
            pending_offset: 0,
            quantized_output: Vec::new(),
            metrics,
        })
    }

    fn poll(&mut self, callback: &mut dyn AsrSinkCallback) -> Result<bool, AsrSinkError> {
        let channels = MASTER_FORMAT.channels.max(1) as usize;
        let bounded_samples = self.consumer.occupied_len() / channels * channels;
        self.poll_with_limit(callback, Some(bounded_samples))
    }

    fn stop_now(&mut self) {
        self.drained_master.clear();
        self.converted_output.clear();
        self.pending_output.clear();
        self.pending_offset = 0;
        self.quantized_output.clear();
        self.metrics.pending_frames.store(0, Ordering::Relaxed);
    }

    fn poll_with_limit(
        &mut self,
        callback: &mut dyn AsrSinkCallback,
        max_samples: Option<usize>,
    ) -> Result<bool, AsrSinkError> {
        let poll_start = Instant::now();
        let mut callback_time_us = 0_u32;
        let mut progressed = false;
        let mut drained = 0_usize;

        while max_samples.map(|limit| drained < limit).unwrap_or(true) {
            let Some(sample) = self.consumer.try_pop() else {
                break;
            };
            self.drained_master.push(sample);
            progressed = true;
            drained += 1;
        }

        let input_channels = MASTER_FORMAT.channels.max(1) as usize;
        let ready_input_samples = self.drained_master.len() / input_channels * input_channels;
        if ready_input_samples > 0 {
            self.converter
                .convert(&self.drained_master[..ready_input_samples], &mut self.converted_output)?;
            if !self.converted_output.is_empty() {
                self.pending_output
                    .extend_from_slice(&self.converted_output);
                progressed = true;
            }

            if ready_input_samples == self.drained_master.len() {
                self.drained_master.clear();
            } else {
                self.drained_master.drain(..ready_input_samples);
            }
        }

        let chunk_samples = self.chunk_frames * self.target_format.channels as usize;
        while self
            .pending_output
            .len()
            .saturating_sub(self.pending_offset)
            >= chunk_samples
        {
            let start = self.pending_offset;
            let end = start + chunk_samples;
            let chunk = &self.pending_output[start..end];

            match self.target_format.sample_format {
                SampleFormat::F32 => {
                    let callback_start = Instant::now();
                    callback.on_chunk(AsrChunkView {
                        input_id: &self.input_id,
                        frames: self.chunk_frames,
                        samples: AsrSampleSlice::F32(chunk),
                    });
                    let elapsed_us = duration_to_u32_us(callback_start.elapsed());
                    self.metrics.callback.record(elapsed_us);
                    callback_time_us = callback_time_us.saturating_add(elapsed_us);
                }
                SampleFormat::I16 => {
                    convert_f32_to_i16(chunk, &mut self.quantized_output);
                    let callback_start = Instant::now();
                    callback.on_chunk(AsrChunkView {
                        input_id: &self.input_id,
                        frames: self.chunk_frames,
                        samples: AsrSampleSlice::I16(&self.quantized_output),
                    });
                    let elapsed_us = duration_to_u32_us(callback_start.elapsed());
                    self.metrics.callback.record(elapsed_us);
                    callback_time_us = callback_time_us.saturating_add(elapsed_us);
                }
            }

            self.metrics.chunks_emitted.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .frames_emitted
                .fetch_add(self.chunk_frames as u64, Ordering::Relaxed);
            self.pending_offset = end;
            progressed = true;
        }

        self.compact_pending();
        self.metrics
            .pending_frames
            .store(self.available_pending_frames() as u32, Ordering::Relaxed);
        if progressed {
            let total_us = duration_to_u32_us(poll_start.elapsed());
            self.metrics
                .poll
                .record(total_us.saturating_sub(callback_time_us));
        }
        Ok(progressed)
    }

    fn compact_pending(&mut self) {
        if self.pending_offset == 0 {
            return;
        }

        if self.pending_offset >= self.pending_output.len() {
            self.pending_output.clear();
            self.pending_offset = 0;
            return;
        }

        if self.pending_offset >= self.pending_output.len() / 2 {
            self.pending_output.drain(..self.pending_offset);
            self.pending_offset = 0;
        }
    }

    fn available_pending_frames(&self) -> usize {
        self.pending_output
            .len()
            .saturating_sub(self.pending_offset)
            / self.target_format.channels as usize
    }
}

#[derive(Default)]
struct AsrSinkMetrics {
    inputs: HashMap<AsrInputId, Arc<AsrInputMetrics>>,
}

impl AsrSinkMetrics {
    fn snapshot(&self) -> AsrSinkMetricsSnapshot {
        self.inputs
            .iter()
            .map(|(input_id, metrics)| (input_id.clone(), metrics.snapshot()))
            .collect()
    }
}

pub struct AsrSink {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Vec<AsrSinkInput>, AsrSinkError>>>,
    metrics: Arc<AsrSinkMetrics>,
}

impl AsrSink {
    pub fn validate_config(config: AsrSinkConfig) -> Result<(), AsrSinkError> {
        if config.chunk_frames == 0 {
            return Err(AsrSinkError::InvalidChunkFrames);
        }

        if !(1..=2).contains(&config.format.channels) {
            return Err(AsrSinkError::UnsupportedOutputChannels(
                config.format.channels,
            ));
        }

        let f32_format = StreamFormat::with_sample_format(
            config.format.sample_rate,
            config.format.channels,
            SampleFormat::F32,
        );
        MasterFormatConverter::new(MASTER_FORMAT, f32_format)?;
        Ok(())
    }

    pub fn try_spawn(
        inputs: Vec<AsrSinkInput>,
        config: AsrSinkConfig,
        mut callback: Box<dyn AsrSinkCallback>,
    ) -> Result<Self, (AsrSinkError, Vec<AsrSinkInput>)> {
        if inputs.is_empty() {
            return Err((AsrSinkError::NoInputs, inputs));
        }

        if let Err(err) = Self::validate_config(config) {
            return Err((err, inputs));
        }

        let mut states = Vec::with_capacity(inputs.len());
        let mut input_metrics = HashMap::with_capacity(inputs.len());
        for input in inputs {
            let metrics = Arc::new(AsrInputMetrics::default());
            input_metrics.insert(input.input_id.clone(), metrics.clone());
            match InputState::new(input, config, metrics) {
                Ok(state) => states.push(state),
                Err(err) => {
                    let inputs = states
                        .into_iter()
                        .map(|state| AsrSinkInput {
                            input_id: state.input_id,
                            consumer: state.consumer,
                        })
                        .collect();
                    return Err((err, inputs));
                }
            }
        }
        let metrics = Arc::new(AsrSinkMetrics {
            inputs: input_metrics,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = thread::spawn(move || -> Result<Vec<AsrSinkInput>, AsrSinkError> {
            let idle_sleep = Duration::from_micros(200);

            loop {
                if stop_thread.load(Ordering::Acquire) {
                    for state in &mut states {
                        state.stop_now();
                    }
                    break;
                }

                let mut progressed = false;
                for state in &mut states {
                    progressed |= state.poll(&mut *callback)?;
                }

                if !progressed {
                    thread::sleep(idle_sleep);
                }
            }

            Ok(states
                .into_iter()
                .map(|state| AsrSinkInput {
                    input_id: state.input_id,
                    consumer: state.consumer,
                })
                .collect())
        });

        Ok(Self {
            stop,
            handle: Some(handle),
            metrics,
        })
    }

    pub fn spawn(
        inputs: Vec<AsrSinkInput>,
        config: AsrSinkConfig,
        callback: Box<dyn AsrSinkCallback>,
    ) -> Result<Self, AsrSinkError> {
        Self::try_spawn(inputs, config, callback).map_err(|(err, _inputs)| err)
    }

    pub fn stats(&self) -> AsrSinkMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn stop(&mut self) -> Result<Vec<AsrSinkInput>, AsrSinkError> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return Err(AsrSinkError::AlreadyStopped);
        };

        match handle.join() {
            Ok(res) => res,
            Err(_) => Err(AsrSinkError::ThreadPanic),
        }
    }
}

fn duration_to_u32_us(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

impl Drop for AsrSink {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AudioEngineController, SourceType};
    use crossbeam_channel::unbounded;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn validate_config_rejects_zero_chunk_frames() {
        let err = AsrSink::validate_config(AsrSinkConfig {
            format: StreamFormat::new(48_000, 1),
            chunk_frames: 0,
        })
        .expect_err("invalid chunk frames");

        assert!(matches!(err, AsrSinkError::InvalidChunkFrames));
    }

    #[test]
    fn validate_config_rejects_unsupported_channels() {
        let err = AsrSink::validate_config(AsrSinkConfig {
            format: StreamFormat::new(48_000, 3),
            chunk_frames: 4,
        })
        .expect_err("unsupported channels");

        assert!(matches!(err, AsrSinkError::UnsupportedOutputChannels(3)));
    }

    #[test]
    fn spawn_rejects_no_inputs() {
        let err = match AsrSink::spawn(
            vec![],
            AsrSinkConfig {
                format: StreamFormat::new(48_000, 1),
                chunk_frames: 4,
            },
            Box::new(|_chunk: AsrChunkView<'_>| {}),
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected no inputs error"),
        };

        assert!(matches!(err, AsrSinkError::NoInputs));
    }

    #[test]
    fn duration_to_u32_us_saturates() {
        assert_eq!(duration_to_u32_us(Duration::MAX), u32::MAX);
    }

    #[test]
    fn emits_f32_chunks_from_stereo_input() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "capture".to_string();
        let output = "asr_capture".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("route");

        let consumer = engine
            .take_output_consumer(&output)
            .expect("output consumer present");

        let (tx, rx) = unbounded();
        let mut sink = AsrSink::spawn(
            vec![AsrSinkInput {
                input_id: "capture".to_string(),
                consumer,
            }],
            AsrSinkConfig {
                format: StreamFormat::new(48_000, 1),
                chunk_frames: 4,
            },
            Box::new(move |chunk: AsrChunkView<'_>| {
                let AsrSampleSlice::F32(samples) = chunk.samples else {
                    panic!("expected f32 samples");
                };
                tx.send((chunk.input_id.to_string(), chunk.frames, samples.to_vec()))
                    .expect("send chunk");
            }),
        )
        .expect("spawn sink");

        let mut frame = [0.0_f32; 8];
        frame.copy_from_slice(&[0.2, 0.2, 0.8, 0.8, -0.5, -0.5, 0.1, 0.1]);
        pipeline.process_callback(&mut frame);

        let (input_id, frames, samples) = rx.recv_timeout(Duration::from_secs(1)).expect("chunk");
        assert_eq!(input_id, "capture");
        assert_eq!(frames, 4);
        assert_eq!(samples, vec![0.2, 0.8, -0.5, 0.1]);

        sink.stop().expect("stop sink");
    }

    #[test]
    fn emits_independent_i16_chunks_for_multiple_inputs() {
        let mut engine = AudioEngineController::new(32, 32, 4096);

        let capture_stream = "capture".to_string();
        let reference_stream = "reference".to_string();
        let capture_output = "asr_capture".to_string();
        let reference_output = "asr_reference".to_string();

        let mut capture_pipeline = engine
            .create_stream(capture_stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create capture stream");
        let mut reference_pipeline = engine
            .create_stream(
                reference_stream.clone(),
                SourceType::Microphone { device_id: None },
                8,
                4,
            )
            .expect("create reference stream");

        engine
            .route(&capture_stream, &capture_output)
            .expect("route capture");
        engine
            .route(&reference_stream, &reference_output)
            .expect("route reference");

        let capture_consumer = engine
            .take_output_consumer(&capture_output)
            .expect("capture consumer");
        let reference_consumer = engine
            .take_output_consumer(&reference_output)
            .expect("reference consumer");

        let (tx, rx) = unbounded();
        let mut sink = AsrSink::spawn(
            vec![
                AsrSinkInput {
                    input_id: "capture".to_string(),
                    consumer: capture_consumer,
                },
                AsrSinkInput {
                    input_id: "reference".to_string(),
                    consumer: reference_consumer,
                },
            ],
            AsrSinkConfig {
                format: StreamFormat::with_sample_format(48_000, 1, SampleFormat::I16),
                chunk_frames: 4,
            },
            Box::new(move |chunk: AsrChunkView<'_>| {
                let AsrSampleSlice::I16(samples) = chunk.samples else {
                    panic!("expected i16 samples");
                };
                tx.send((chunk.input_id.to_string(), samples.to_vec()))
                    .expect("send chunk");
            }),
        )
        .expect("spawn sink");

        let mut capture_frame = [0.0_f32; 8];
        capture_frame.copy_from_slice(&[0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]);
        capture_pipeline.process_callback(&mut capture_frame);

        let mut reference_frame = [0.0_f32; 8];
        reference_frame.copy_from_slice(&[-0.25, -0.25, -0.25, -0.25, -0.25, -0.25, -0.25, -0.25]);
        reference_pipeline.process_callback(&mut reference_frame);

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let second = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second chunk");

        let mut seen = vec![first, second];
        seen.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(seen[0].0, "capture");
        assert_eq!(seen[0].1, vec![16384, 16384, 16384, 16384]);
        assert_eq!(seen[1].0, "reference");
        assert_eq!(seen[1].1, vec![-8192, -8192, -8192, -8192]);

        sink.stop().expect("stop sink");
    }

    #[test]
    fn stop_twice_returns_already_stopped() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "capture".to_string();
        let output = "asr_capture".to_string();

        let _pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("route");

        let consumer = engine
            .take_output_consumer(&output)
            .expect("output consumer present");

        let mut sink = AsrSink::spawn(
            vec![AsrSinkInput {
                input_id: output,
                consumer,
            }],
            AsrSinkConfig {
                format: StreamFormat::new(48_000, 1),
                chunk_frames: 4,
            },
            Box::new(|_chunk: AsrChunkView<'_>| {}),
        )
        .expect("spawn sink");

        sink.stop().expect("first stop");
        assert!(matches!(sink.stop(), Err(AsrSinkError::AlreadyStopped)));
    }

    #[test]
    fn exposes_per_input_metrics_snapshots() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "capture".to_string();
        let output = "asr_capture".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("route");

        let consumer = engine
            .take_output_consumer(&output)
            .expect("output consumer present");

        let (tx, rx) = unbounded();
        let mut sink = AsrSink::spawn(
            vec![AsrSinkInput {
                input_id: output.clone(),
                consumer,
            }],
            AsrSinkConfig {
                format: StreamFormat::with_sample_format(48_000, 1, SampleFormat::I16),
                chunk_frames: 4,
            },
            Box::new(move |chunk: AsrChunkView<'_>| {
                let AsrSampleSlice::I16(samples) = chunk.samples else {
                    panic!("expected i16 samples");
                };
                tx.send(samples.to_vec()).expect("send chunk");
            }),
        )
        .expect("spawn sink");

        let mut frame = [0.0_f32; 8];
        frame.copy_from_slice(&[0.25, 0.25, 0.5, 0.5, -0.25, -0.25, 0.0, 0.0]);
        pipeline.process_callback(&mut frame);

        rx.recv_timeout(Duration::from_secs(1)).expect("chunk");

        let metrics = sink.stats();
        let input_metrics = metrics.get(&output).expect("input metrics present");
        assert_eq!(input_metrics.chunks_emitted, 1);
        assert_eq!(input_metrics.frames_emitted, 4);
        assert_eq!(input_metrics.pending_frames, 0);
        assert!(input_metrics.poll.count >= 1);
        assert!(input_metrics.callback.count >= 1);

        sink.stop().expect("stop sink");
    }

    #[test]
    fn stop_does_not_keep_emitting_future_live_chunks() {
        let ring = HeapRb::<f32>::new(4096);
        let (mut producer, consumer) = ring.split();
        let chunk_count = Arc::new(AtomicUsize::new(0));
        let chunk_count_callback = chunk_count.clone();

        let sink = AsrSink::spawn(
            vec![AsrSinkInput {
                input_id: "live".to_string(),
                consumer,
            }],
            AsrSinkConfig {
                format: StreamFormat::new(48_000, 1),
                chunk_frames: 4,
            },
            Box::new(move |_chunk: AsrChunkView<'_>| {
                chunk_count_callback.fetch_add(1, AtomicOrdering::Relaxed);
            }),
        )
        .expect("spawn sink");

        let stop_producers = Arc::new(AtomicBool::new(false));
        let stop_flag = stop_producers.clone();
        let producer_thread = thread::spawn(move || {
            let batch = [1.0_f32; 8];
            while !stop_flag.load(AtomicOrdering::Relaxed) {
                let _ = producer.push_slice(&batch);
                thread::sleep(Duration::from_millis(2));
            }
        });

        thread::sleep(Duration::from_millis(25));

        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut sink = sink;
            let started = Instant::now();
            let result = sink.stop();
            done_tx
                .send((started.elapsed(), result))
                .expect("send stop result");
        });

        let (elapsed, result) = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop sink should not block on live input");
        result.expect("stop sink");
        stop_producers.store(true, AtomicOrdering::Relaxed);
        producer_thread.join().expect("join producer thread");

        assert!(elapsed < Duration::from_secs(1));
        assert!(chunk_count.load(AtomicOrdering::Relaxed) > 0);
    }
}
