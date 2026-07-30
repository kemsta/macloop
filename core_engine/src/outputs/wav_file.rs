use crate::converter::{
    convert_f32_to_i16, InputConversionError, InputConverter, MasterFormatConverter,
};
use crate::engine::RouteConsumer;
use crate::format::{SampleFormat, StreamFormat, MASTER_FORMAT};
use crate::metrics::{LatencyHistogram, LatencyHistogramSnapshot};
use ringbuf::traits::{Consumer, Observer};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum WavOutputError {
    UnsupportedSampleFormat(SampleFormat),
    UnsupportedOutputChannels(u16),
    Converter(InputConversionError),
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
            Self::UnsupportedOutputChannels(channels) => {
                write!(f, "unsupported output channels for wav sink: {channels}")
            }
            Self::Converter(err) => write!(f, "converter error: {err}"),
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

impl From<InputConversionError> for WavOutputError {
    fn from(value: InputConversionError) -> Self {
        Self::Converter(value)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WavSinkConfig {
    pub format: StreamFormat,
    pub mix_gain: f32,
}

impl Default for WavSinkConfig {
    fn default() -> Self {
        Self {
            format: MASTER_FORMAT,
            mix_gain: 1.0,
        }
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

struct WavThreadResult {
    consumers: Vec<RouteConsumer>,
    result: Result<(), WavOutputError>,
}

pub struct WavFileOutput {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<WavThreadResult>>,
    metrics: Arc<WavSinkMetrics>,
}

impl WavFileOutput {
    pub fn validate_config(config: WavSinkConfig) -> Result<(), WavOutputError> {
        if !(1..=2).contains(&config.format.channels) {
            return Err(WavOutputError::UnsupportedOutputChannels(
                config.format.channels,
            ));
        }

        let f32_format = StreamFormat::with_sample_format(
            config.format.sample_rate,
            config.format.channels,
            SampleFormat::F32,
        );
        MasterFormatConverter::new(MASTER_FORMAT, f32_format)?;
        Self::wav_spec(config.format)?;
        Ok(())
    }

    pub fn try_spawn_mix_with_config<W>(
        writer: W,
        consumers: Vec<RouteConsumer>,
        config: WavSinkConfig,
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

        if let Err(err) = Self::validate_config(config) {
            return Err((err, consumers));
        }

        let spec = match Self::wav_spec(config.format) {
            Ok(spec) => spec,
            Err(err) => return Err((err, consumers)),
        };
        let f32_format = StreamFormat::with_sample_format(
            config.format.sample_rate,
            config.format.channels,
            SampleFormat::F32,
        );
        let mut converter = match MasterFormatConverter::new(MASTER_FORMAT, f32_format) {
            Ok(converter) => converter,
            Err(err) => return Err((WavOutputError::Converter(err), consumers)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let metrics = Arc::new(WavSinkMetrics::default());
        let metrics_thread = metrics.clone();
        let frame_channels = MASTER_FORMAT.channels as usize;

        let handle = thread::spawn(move || {
            let mut consumers = consumers;
            let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), WavOutputError> {
                let mut writer = hound::WavWriter::new(writer, spec)?;
                let idle_sleep = Duration::from_micros(200);
                let mut input_buffers = vec![VecDeque::<f32>::new(); consumers.len()];
                let mut mixed_buffer = Vec::<f32>::new();
                let mut converted_buffer = Vec::<f32>::new();
                let mut quantized_buffer = Vec::<i16>::new();

                loop {
                    let stopping = stop_thread.load(Ordering::Acquire);
                    let mut drained_any = false;
                    for (consumer, buffer) in consumers.iter_mut().zip(input_buffers.iter_mut()) {
                        let drain_limit = consumer.occupied_len() / frame_channels * frame_channels;
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

                    let ready_samples = input_buffers.iter().map(VecDeque::len).min().unwrap_or(0)
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
                            mixed_buffer.push(mixed_sample * config.mix_gain);
                        }

                        converter.convert(&mixed_buffer, &mut converted_buffer)?;
                        write_output_samples(
                            &mut writer,
                            &converted_buffer,
                            config.format,
                            &mut quantized_buffer,
                            &metrics_thread,
                        )?;
                    }

                    if stopping {
                        converter.finish(&mut converted_buffer)?;
                        write_output_samples(
                            &mut writer,
                            &converted_buffer,
                            config.format,
                            &mut quantized_buffer,
                            &metrics_thread,
                        )?;
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
                let finalize_result = writer.finalize().map_err(WavOutputError::from);
                metrics_thread
                    .finalize
                    .record(duration_to_u32_us(finalize_start.elapsed()));
                finalize_result
            }))
            .unwrap_or(Err(WavOutputError::ThreadPanic));

            WavThreadResult { consumers, result }
        });

        Ok(Self {
            stop,
            handle: Some(handle),
            metrics,
        })
    }

    pub fn try_spawn_mix<W>(
        writer: W,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, (WavOutputError, Vec<RouteConsumer>)>
    where
        W: Write + Seek + Send + 'static,
    {
        Self::try_spawn_mix_with_config(writer, consumers, WavSinkConfig { format, mix_gain })
    }

    pub fn spawn_mix_with_config<W>(
        writer: W,
        consumers: Vec<RouteConsumer>,
        config: WavSinkConfig,
    ) -> Result<Self, WavOutputError>
    where
        W: Write + Seek + Send + 'static,
    {
        Self::try_spawn_mix_with_config(writer, consumers, config).map_err(|(err, _consumers)| err)
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

    pub fn try_spawn_file_mix_with_config(
        file: File,
        consumers: Vec<RouteConsumer>,
        config: WavSinkConfig,
    ) -> Result<Self, (WavOutputError, Vec<RouteConsumer>)> {
        Self::try_spawn_mix_with_config(BufWriter::new(file), consumers, config)
    }

    pub fn try_spawn_file_mix(
        file: File,
        format: StreamFormat,
        consumers: Vec<RouteConsumer>,
        mix_gain: f32,
    ) -> Result<Self, (WavOutputError, Vec<RouteConsumer>)> {
        Self::try_spawn_mix(BufWriter::new(file), format, consumers, mix_gain)
    }

    pub fn spawn_file_mix_with_config(
        file: File,
        consumers: Vec<RouteConsumer>,
        config: WavSinkConfig,
    ) -> Result<Self, WavOutputError> {
        Self::spawn_mix_with_config(BufWriter::new(file), consumers, config)
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

    pub fn spawn_path_mix_with_config<P: AsRef<Path>>(
        path: P,
        consumers: Vec<RouteConsumer>,
        config: WavSinkConfig,
    ) -> Result<Self, WavOutputError> {
        let file = File::create(path)?;
        Self::spawn_file_mix_with_config(file, consumers, config)
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
        let (sample_format, bits_per_sample) = match format.sample_format {
            SampleFormat::F32 => (hound::SampleFormat::Float, 32),
            SampleFormat::I16 => (hound::SampleFormat::Int, 16),
        };

        Ok(hound::WavSpec {
            channels: format.channels,
            sample_rate: format.sample_rate,
            bits_per_sample,
            sample_format,
        })
    }

    pub fn stats(&self) -> WavSinkMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn stop_with_consumers(
        &mut self,
    ) -> Result<Vec<RouteConsumer>, (WavOutputError, Vec<RouteConsumer>)> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return Err((WavOutputError::AlreadyStopped, Vec::new()));
        };

        match handle.join() {
            Ok(WavThreadResult {
                consumers,
                result: Ok(()),
            }) => Ok(consumers),
            Ok(WavThreadResult {
                consumers,
                result: Err(err),
            }) => Err((err, consumers)),
            Err(_) => Err((WavOutputError::ThreadPanic, Vec::new())),
        }
    }

    pub fn stop(&mut self) -> Result<Vec<RouteConsumer>, WavOutputError> {
        self.stop_with_consumers().map_err(|(err, _consumers)| err)
    }
}

fn write_output_samples<W>(
    writer: &mut hound::WavWriter<W>,
    samples: &[f32],
    format: StreamFormat,
    quantized: &mut Vec<i16>,
    metrics: &WavSinkMetrics,
) -> Result<(), WavOutputError>
where
    W: Write + Seek,
{
    if samples.is_empty() {
        return Ok(());
    }

    let write_start = Instant::now();
    match format.sample_format {
        SampleFormat::F32 => {
            for sample in samples {
                writer.write_sample(*sample)?;
            }
        }
        SampleFormat::I16 => {
            convert_f32_to_i16(samples, quantized);
            for sample in quantized {
                writer.write_sample(*sample)?;
            }
        }
    }

    let samples_written = samples.len() as u64;
    metrics.write_calls.fetch_add(1, Ordering::Relaxed);
    metrics
        .samples_written
        .fetch_add(samples_written, Ordering::Relaxed);
    metrics
        .frames_written
        .fetch_add(samples_written / format.channels as u64, Ordering::Relaxed);
    metrics
        .write
        .record(duration_to_u32_us(write_start.elapsed()));
    Ok(())
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
    use std::io::{self, Cursor, SeekFrom};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_wav_path() -> PathBuf {
        static NEXT_PATH_ID: AtomicU64 = AtomicU64::new(0);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path_id = NEXT_PATH_ID.fetch_add(1, AtomicOrdering::Relaxed);
        std::env::temp_dir().join(format!("core_engine_wav_test_{timestamp}_{path_id}.wav"))
    }

    struct WriteFailingWriter;

    impl Write for WriteFailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "test writer rejected write",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for WriteFailingWriter {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    struct PanickingWriter {
        inner: Cursor<Vec<u8>>,
        panic_next_write: bool,
    }

    impl PanickingWriter {
        fn new() -> Self {
            Self {
                inner: Cursor::new(Vec::new()),
                panic_next_write: true,
            }
        }
    }

    impl Write for PanickingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.panic_next_write) {
                panic!("test writer panicked");
            }
            self.inner.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for PanickingWriter {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[derive(Default)]
    struct FinalizeFailingWriter {
        inner: Cursor<Vec<u8>>,
    }

    impl Write for FinalizeFailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.inner.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl Seek for FinalizeFailingWriter {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "test writer rejected finalize seek",
            ))
        }
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
    fn write_error_returns_consumers_for_reuse() {
        let ring = HeapRb::<f32>::new(32);
        let (mut producer, consumer) = ring.split();
        let mut failing =
            WavFileOutput::spawn_mix(WriteFailingWriter, MASTER_FORMAT, vec![consumer], 1.0)
                .expect("spawn failing wav");

        let (err, consumers) = match failing.stop_with_consumers() {
            Err(result) => result,
            Ok(_) => panic!("writer should fail"),
        };
        assert!(matches!(err, WavOutputError::Hound(_)));
        assert_eq!(consumers.len(), 1);

        assert_eq!(producer.push_slice(&[0.25_f32; 8]), 8);
        let mut reused =
            WavFileOutput::spawn_mix(Cursor::new(Vec::<u8>::new()), MASTER_FORMAT, consumers, 1.0)
                .expect("reuse returned consumer");
        reused.stop().expect("stop reused wav");
    }

    #[test]
    fn writer_panic_returns_all_consumers_for_reuse() {
        let ring_a = HeapRb::<f32>::new(32);
        let ring_b = HeapRb::<f32>::new(32);
        let (mut producer_a, consumer_a) = ring_a.split();
        let (mut producer_b, consumer_b) = ring_b.split();
        let mut panicking = WavFileOutput::spawn_mix(
            PanickingWriter::new(),
            MASTER_FORMAT,
            vec![consumer_a, consumer_b],
            1.0,
        )
        .expect("spawn panicking wav");

        let (err, consumers) = match panicking.stop_with_consumers() {
            Err(result) => result,
            Ok(_) => panic!("writer should panic"),
        };
        assert!(matches!(err, WavOutputError::ThreadPanic));
        assert_eq!(consumers.len(), 2);

        assert_eq!(producer_a.push_slice(&[0.25_f32; 8]), 8);
        assert_eq!(producer_b.push_slice(&[0.75_f32; 8]), 8);
        let mut reused =
            WavFileOutput::spawn_mix(Cursor::new(Vec::<u8>::new()), MASTER_FORMAT, consumers, 1.0)
                .expect("reuse returned consumers");
        assert_eq!(reused.stop().expect("stop reused wav").len(), 2);
    }

    #[test]
    fn finalize_error_returns_consumers_and_records_stats() {
        let ring = HeapRb::<f32>::new(32);
        let (_producer, consumer) = ring.split();
        let mut wav = WavFileOutput::spawn_mix(
            FinalizeFailingWriter::default(),
            MASTER_FORMAT,
            vec![consumer],
            1.0,
        )
        .expect("spawn finalize-failing wav");

        let (err, consumers) = match wav.stop_with_consumers() {
            Err(result) => result,
            Ok(_) => panic!("finalize should fail"),
        };

        assert!(matches!(err, WavOutputError::Hound(_)));
        assert_eq!(consumers.len(), 1);
        assert_eq!(wav.stats().finalize.count, 1);
    }

    #[test]
    fn wav_spec_supports_i16_output() {
        let spec = WavFileOutput::wav_spec(StreamFormat::with_sample_format(
            16_000,
            1,
            SampleFormat::I16,
        ))
        .expect("i16 spec");

        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
    }

    #[test]
    fn validate_config_rejects_unsupported_channels() {
        let err = WavFileOutput::validate_config(WavSinkConfig {
            format: StreamFormat::new(48_000, 3),
            mix_gain: 1.0,
        })
        .expect_err("unsupported channels");

        assert!(matches!(err, WavOutputError::UnsupportedOutputChannels(3)));
    }

    #[test]
    fn duration_to_u32_us_saturates() {
        assert_eq!(duration_to_u32_us(Duration::MAX), u32::MAX);
    }

    #[test]
    fn writes_default_master_f32_wav_from_routed_stream_to_file() {
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
    fn converts_master_stereo_to_16khz_mono_i16() {
        let path = test_wav_path();

        let mut engine = AudioEngineController::new(32, 32, 32_768);
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
        let mut wav = WavFileOutput::spawn_file_mix_with_config(
            file,
            vec![consumer],
            WavSinkConfig {
                format: StreamFormat::with_sample_format(16_000, 1, SampleFormat::I16),
                mix_gain: 1.0,
            },
        )
        .expect("spawn converted wav output");

        let mut frame = [0.25_f32; 320];
        for _ in 0..30 {
            pipeline.process_callback(&mut frame);
        }
        wav.stop().expect("stop wav output");
        let stats = wav.stats();

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|sample| sample.expect("sample"))
            .collect();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(samples.len(), 1_600);
        assert!(samples[500..samples.len() - 200]
            .iter()
            .all(|sample| (*sample - 8192).abs() <= 2));
        assert_eq!(stats.samples_written, samples.len() as u64);
        assert_eq!(stats.frames_written, samples.len() as u64);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn converted_wav_preserves_final_impulse_tail() {
        let path = test_wav_path();
        let ring = HeapRb::<f32>::new(10_000);
        let (mut producer, consumer) = ring.split();
        let file = File::create(&path).expect("create output file");
        let mut wav = WavFileOutput::spawn_file_mix_with_config(
            file,
            vec![consumer],
            WavSinkConfig {
                format: StreamFormat::with_sample_format(16_000, 1, SampleFormat::F32),
                mix_gain: 1.0,
            },
        )
        .expect("spawn converted wav output");
        let mut input = vec![0.0_f32; 4_800 * MASTER_FORMAT.channels as usize];
        let last_frame = input.len() - MASTER_FORMAT.channels as usize;
        input[last_frame..].fill(1.0);
        assert_eq!(producer.push_slice(&input), input.len());

        wav.stop().expect("stop wav output");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<f32>()
            .map(|sample| sample.expect("sample"))
            .collect();
        let peak_index = samples
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
            .map(|(index, _)| index)
            .expect("wav samples");
        assert_eq!(samples.len(), 1_600);
        assert!(
            peak_index >= samples.len() - 2,
            "final impulse was truncated"
        );
        assert!(samples[samples.len() - 4..]
            .iter()
            .any(|sample| sample.abs() > 0.1));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn explicit_mix_gain_one_sums_routes_before_conversion() {
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
        let mut wav = WavFileOutput::spawn_file_mix_with_config(
            file,
            vec![consumer_a, consumer_b],
            WavSinkConfig {
                format: StreamFormat::with_sample_format(48_000, 1, SampleFormat::I16),
                mix_gain: 1.0,
            },
        )
        .expect("spawn mixed wav output");

        let mut frame = [0.25_f32; 8];
        pipeline.process_callback(&mut frame);
        wav.stop().expect("stop wav output");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|sample| sample.expect("sample"))
            .collect();
        assert_eq!(samples, vec![16384; 4]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn pcm16_output_clips_mixed_samples_safely() {
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
        let mut wav = WavFileOutput::spawn_file_mix_with_config(
            file,
            vec![consumer_a, consumer_b],
            WavSinkConfig {
                format: StreamFormat::with_sample_format(48_000, 1, SampleFormat::I16),
                mix_gain: 1.0,
            },
        )
        .expect("spawn mixed wav output");

        let mut frame = [0.75_f32, 0.75, -0.75, -0.75];
        pipeline.process_callback(&mut frame);
        wav.stop().expect("stop wav output");

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|sample| sample.expect("sample"))
            .collect();
        assert_eq!(samples, vec![i16::MAX, i16::MIN + 1]);

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
        assert!(matches!(wav.stop(), Err(WavOutputError::AlreadyStopped)));
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
