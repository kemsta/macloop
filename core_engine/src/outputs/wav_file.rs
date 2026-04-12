use crate::engine::RouteConsumer;
use crate::format::{SampleFormat, StreamFormat};
use crate::metrics::{LatencyHistogram, LatencyHistogramSnapshot};
use ringbuf::traits::{Consumer, Observer};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum WavOutputError {
    UnsupportedSampleFormat(SampleFormat),
    Io(String),
    Hound(String),
    ThreadPanic,
    AlreadyStopped,
}

impl std::fmt::Display for WavOutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSampleFormat(fmt) => {
                write!(f, "unsupported WAV sample format: {:?}", fmt)
            }
            Self::Io(err) => write!(f, "wav io error: {err}"),
            Self::Hound(err) => write!(f, "wav writer error: {err}"),
            Self::ThreadPanic => write!(f, "wav writer thread panicked"),
            Self::AlreadyStopped => write!(f, "wav writer already stopped"),
        }
    }
}

impl std::error::Error for WavOutputError {}

impl From<std::io::Error> for WavOutputError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<hound::Error> for WavOutputError {
    fn from(value: hound::Error) -> Self {
        Self::Hound(value.to_string())
    }
}

pub struct WavSinkMetrics {
    write_calls: AtomicU64,
    samples_written: AtomicU64,
    frames_written: AtomicU64,
    write: LatencyHistogram,
    finalize: LatencyHistogram,
}

impl Default for WavSinkMetrics {
    fn default() -> Self {
        Self {
            write_calls: AtomicU64::new(0),
            samples_written: AtomicU64::new(0),
            frames_written: AtomicU64::new(0),
            write: LatencyHistogram::default(),
            finalize: LatencyHistogram::default(),
        }
    }
}

impl WavSinkMetrics {
    fn snapshot(&self) -> WavSinkMetricsSnapshot {
        WavSinkMetricsSnapshot {
            write_calls: self.write_calls.load(Ordering::Relaxed),
            samples_written: self.samples_written.load(Ordering::Relaxed),
            frames_written: self.frames_written.load(Ordering::Relaxed),
            write: self.write.snapshot(),
            finalize: self.finalize.snapshot(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WavSinkMetricsSnapshot {
    pub write_calls: u64,
    pub samples_written: u64,
    pub frames_written: u64,
    pub write: LatencyHistogramSnapshot,
    pub finalize: LatencyHistogramSnapshot,
}

pub struct WavFileOutput {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Vec<RouteConsumer>, WavOutputError>>>,
    metrics: Arc<WavSinkMetrics>,
}

impl WavFileOutput {
    pub fn try_spawn_mix<W>(
        writer: W,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, (WavOutputError, Vec<RouteConsumer>)>
    where
        W: Write + Seek + Send + 'static,
    {
        if consumers.is_empty() {
            return Err((
                WavOutputError::Io("wav sink requires at least one route consumer".to_string()),
                consumers,
            ));
        }

        let spec = match Self::wav_spec(format) {
            Ok(spec) => spec,
            Err(err) => return Err((err, consumers)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let metrics = Arc::new(WavSinkMetrics::default());
        let metrics_thread = metrics.clone();
        let channels = format.channels.max(1) as u64;
        let frame_channels = format.channels.max(1) as usize;

        let handle = thread::spawn(move || -> Result<Vec<RouteConsumer>, WavOutputError> {
            let mut writer = hound::WavWriter::new(writer, spec)?;
            let idle_sleep = Duration::from_micros(200);
            let mut consumers = consumers;
            let mut input_buffers = vec![VecDeque::<f32>::new(); consumers.len()];
            let mut mixed_buffer = Vec::<f32>::new();

            loop {
                let stopping = stop_thread.load(Ordering::Relaxed);
                let mut drained_any = false;
                for (consumer, buffer) in consumers.iter_mut().zip(input_buffers.iter_mut()) {
                    let drain_limit = if stopping {
                        consumer.occupied_len() / frame_channels * frame_channels
                    } else {
                        usize::MAX
                    };
                    let mut drained = 0_usize;
                    while drained < drain_limit {
                        let Some(sample) = consumer.try_pop() else {
                            break;
                        };
                        buffer.push_back(sample);
                        drained_any = true;
                        drained += 1;
                    }
                }

                let ready_samples = input_buffers
                    .iter()
                    .map(VecDeque::len)
                    .min()
                    .unwrap_or(0)
                    / frame_channels
                    * frame_channels;

                if ready_samples > 0 {
                    mixed_buffer.clear();
                    mixed_buffer.reserve(ready_samples);

                    for _ in 0..ready_samples {
                        let mut mixed_sample = 0.0_f32;
                        for input in &mut input_buffers {
                            if let Some(sample) = input.pop_front() {
                                mixed_sample += sample;
                            }
                        }
                        mixed_buffer.push(mixed_sample * mix_gain);
                    }

                    let write_start = Instant::now();
                    for sample in &mixed_buffer {
                        writer.write_sample(*sample)?;
                    }

                    let samples_written = mixed_buffer.len() as u64;
                    metrics_thread.write_calls.fetch_add(1, Ordering::Relaxed);
                    metrics_thread
                        .samples_written
                        .fetch_add(samples_written, Ordering::Relaxed);
                    metrics_thread
                        .frames_written
                        .fetch_add(samples_written / channels, Ordering::Relaxed);
                    metrics_thread
                        .write
                        .record(duration_to_u32_us(write_start.elapsed()));
                }

                if stopping {
                    for input in &mut input_buffers {
                        input.clear();
                    }
                    break;
                }

                if !drained_any && ready_samples == 0 {
                    thread::sleep(idle_sleep);
                }
            }

            let finalize_start = Instant::now();
            writer.finalize()?;
            metrics_thread
                .finalize
                .record(duration_to_u32_us(finalize_start.elapsed()));
            Ok(consumers)
        });

        Ok(Self {
            stop,
            handle: Some(handle),
            metrics,
        })
    }

    pub fn spawn_mix<W>(
        writer: W,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, WavOutputError>
    where
        W: Write + Seek + Send + 'static,
    {
        Self::try_spawn_mix(writer, format, consumers, mix_gain).map_err(|(err, _consumers)| err)
    }

    pub fn spawn<W>(
        writer: W,
        format: StreamFormat,
        consumer: RouteConsumer,
    ) -> Result<Self, WavOutputError>
    where
        W: Write + Seek + Send + 'static,
    {
        Self::spawn_mix(writer, format, vec![consumer], 1.0)
    }

    pub fn spawn_file(
        file: File,
        format: StreamFormat,
        consumer: RouteConsumer,
    ) -> Result<Self, WavOutputError> {
        Self::spawn(BufWriter::new(file), format, consumer)
    }

    pub fn try_spawn_file_mix(
        file: File,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, (WavOutputError, Vec<RouteConsumer>)> {
        Self::try_spawn_mix(BufWriter::new(file), format, consumers, mix_gain)
    }

    pub fn spawn_file_mix(
        file: File,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, WavOutputError> {
        Self::spawn_mix(BufWriter::new(file), format, consumers, mix_gain)
    }

    pub fn spawn_path<P: AsRef<Path>>(
        path: P,
        format: StreamFormat,
        consumer: RouteConsumer,
    ) -> Result<Self, WavOutputError> {
        let file = File::create(path)?;
        Self::spawn_file(file, format, consumer)
    }

    pub fn spawn_path_mix<P: AsRef<Path>>(
        path: P,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, WavOutputError> {
        let file = File::create(path)?;
        Self::spawn_file_mix(file, format, consumers, mix_gain)
    }

    fn wav_spec(format: StreamFormat) -> Result<hound::WavSpec, WavOutputError> {
        let sample_format = match format.sample_format {
            SampleFormat::F32 => hound::SampleFormat::Float,
            SampleFormat::I16 => {
                return Err(WavOutputError::UnsupportedSampleFormat(SampleFormat::I16))
            }
        };

        Ok(hound::WavSpec {
            channels: format.channels,
            sample_rate: format.sample_rate,
            bits_per_sample: 32,
            sample_format,
        })
    }

    pub fn stats(&self) -> WavSinkMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn stop(&mut self) -> Result<Vec<RouteConsumer>, WavOutputError> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return Err(WavOutputError::AlreadyStopped);
        };

        match handle.join() {
            Ok(res) => res,
            Err(_) => Err(WavOutputError::ThreadPanic),
        }
    }
}

fn duration_to_u32_us(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

impl Drop for WavFileOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AudioEngineController, SourceType};
    use crate::format::MASTER_FORMAT;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_wav_path() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("core_engine_wav_test_{suffix}.wav"))
    }

    #[test]
    fn try_spawn_mix_rejects_empty_consumers() {
        let err = match WavFileOutput::try_spawn_mix(
            std::io::Cursor::new(Vec::<u8>::new()),
            MASTER_FORMAT,
            vec![],
            1.0,
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected empty consumers error"),
        };

        assert!(matches!(err.0, WavOutputError::Io(_)));
        assert!(err.1.is_empty());
    }

    #[test]
    fn wav_spec_rejects_i16_output() {
        let err = WavFileOutput::wav_spec(StreamFormat::with_sample_format(
            48_000,
            2,
            SampleFormat::I16,
        ))
        .expect_err("i16 unsupported");

        assert!(matches!(
            err,
            WavOutputError::UnsupportedSampleFormat(SampleFormat::I16)
        ));
    }

    #[test]
    fn duration_to_u32_us_saturates() {
        assert_eq!(duration_to_u32_us(Duration::MAX), u32::MAX);
    }

    #[test]
    fn writes_wav_from_routed_stream_to_file() {
        let path = test_wav_path();

        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "capture".to_string();
        let output = "wav".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine.route(&stream, &output).expect("route output");

        let consumer = engine
            .take_output_consumer(&output)
            .expect("output consumer present");
        let file = File::create(&path).expect("create output file");
        let mut wav =
            WavFileOutput::spawn_file(file, MASTER_FORMAT, consumer).expect("spawn wav output");

        let mut frame = [0.25_f32; 512];
        for _ in 0..8 {
            pipeline.process_callback(&mut frame);
        }

        wav.stop().expect("stop wav output");
        let stats = wav.stats();
        assert_eq!(stats.samples_written, 512 * 8);
        assert_eq!(
            stats.frames_written,
            (512 * 8) / MASTER_FORMAT.channels as u64
        );
        assert!(stats.write_calls >= 1);
        assert!(stats.write.count >= 1);
        assert_eq!(stats.finalize.count, 1);

        let reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, MASTER_FORMAT.channels);
        assert_eq!(spec.sample_rate, MASTER_FORMAT.sample_rate);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        assert!(reader.duration() > 0);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn mixes_multiple_routes_with_mix_gain() {
        let path = test_wav_path();

        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "capture".to_string();
        let output_a = "wav_a".to_string();
        let output_b = "wav_b".to_string();

        let mut pipeline = engine
            .create_stream(stream.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream");
        engine.route(&stream, &output_a).expect("route output a");
        engine.route(&stream, &output_b).expect("route output b");

        let consumer_a = engine
            .take_output_consumer(&output_a)
            .expect("output consumer a present");
        let consumer_b = engine
            .take_output_consumer(&output_b)
            .expect("output consumer b present");
        let file = File::create(&path).expect("create output file");
        let mut wav =
            WavFileOutput::spawn_file_mix(file, MASTER_FORMAT, vec![consumer_a, consumer_b], 0.5)
                .expect("spawn mixed wav output");

        let mut frame = [0.25_f32; 8];
        pipeline.process_callback(&mut frame);

        wav.stop().expect("stop wav output");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .take(8)
            .map(|sample| sample.expect("sample"))
            .collect();
        assert_eq!(samples, vec![0.25; 8]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn mixes_only_aligned_prefix_for_independent_routes() {
        let path = test_wav_path();

        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream_a = "capture_a".to_string();
        let stream_b = "capture_b".to_string();
        let output_a = "wav_a".to_string();
        let output_b = "wav_b".to_string();

        let mut pipeline_a = engine
            .create_stream(stream_a.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream a");
        let mut pipeline_b = engine
            .create_stream(stream_b.clone(), SourceType::SystemAudio, 8, 4)
            .expect("create stream b");
        engine.route(&stream_a, &output_a).expect("route output a");
        engine.route(&stream_b, &output_b).expect("route output b");

        let consumer_a = engine
            .take_output_consumer(&output_a)
            .expect("output consumer a present");
        let consumer_b = engine
            .take_output_consumer(&output_b)
            .expect("output consumer b present");
        let file = File::create(&path).expect("create output file");
        let mut wav =
            WavFileOutput::spawn_file_mix(file, MASTER_FORMAT, vec![consumer_a, consumer_b], 0.5)
                .expect("spawn mixed wav output");

        let mut frame_a = [1.0_f32; 8];
        let mut frame_b = [3.0_f32; 8];

        pipeline_a.process_callback(&mut frame_a);
        std::thread::sleep(Duration::from_millis(10));
        pipeline_b.process_callback(&mut frame_b);

        wav.stop().expect("stop wav output");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .take(8)
            .map(|sample| sample.expect("sample"))
            .collect();
        assert_eq!(samples, vec![2.0; 8]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn stop_twice_returns_already_stopped() {
        let ring = HeapRb::<f32>::new(32);
        let (_producer, consumer) = ring.split();
        let mut wav = WavFileOutput::spawn_mix(
            std::io::Cursor::new(Vec::<u8>::new()),
            MASTER_FORMAT,
            vec![consumer],
            1.0,
        )
        .expect("spawn wav");

        wav.stop().expect("first stop");
        let err = wav.stop().expect_err("second stop should fail");
        assert!(matches!(err, WavOutputError::AlreadyStopped));
    }

    #[test]
    fn stop_does_not_keep_recording_future_live_samples() {
        let path = test_wav_path();

        let ring_a = HeapRb::<f32>::new(4096);
        let ring_b = HeapRb::<f32>::new(4096);
        let (mut producer_a, consumer_a) = ring_a.split();
        let (mut producer_b, consumer_b) = ring_b.split();

        let file = File::create(&path).expect("create output file");
        let mut wav =
            WavFileOutput::spawn_file_mix(file, MASTER_FORMAT, vec![consumer_a, consumer_b], 0.5)
                .expect("spawn mixed wav output");

        let stop_producers = Arc::new(AtomicBool::new(false));
        let stop_flag = stop_producers.clone();
        let producer_thread = thread::spawn(move || {
            let batch_a = [1.0_f32; 8];
            let batch_b = [3.0_f32; 8];

            while !stop_flag.load(AtomicOrdering::Relaxed) {
                let _ = producer_a.push_slice(&batch_a);
                let _ = producer_b.push_slice(&batch_b);
                thread::sleep(Duration::from_millis(2));
            }
        });

        thread::sleep(Duration::from_millis(25));
        wav.stop().expect("stop wav output");
        stop_producers.store(true, AtomicOrdering::Relaxed);
        producer_thread.join().expect("join producer thread");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .map(|sample| sample.expect("sample"))
            .collect();

        assert!(!samples.is_empty());
        assert!(
            samples.len() < 400,
            "unexpectedly recorded too many samples"
        );
        assert!(samples
            .iter()
            .all(|sample| (*sample - 2.0).abs() < f32::EPSILON));

        let _ = fs::remove_file(path);
    }
}
