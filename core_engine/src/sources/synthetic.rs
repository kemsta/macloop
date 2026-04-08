use crate::engine::RealTimePipeline;
use crate::format::MASTER_FORMAT;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SyntheticSourceConfig {
    pub frames_per_callback: usize,
    pub callback_count: usize,
    pub start_value: f32,
    pub step_value: f32,
    pub interval: Duration,
    pub start_delay: Duration,
}

impl Default for SyntheticSourceConfig {
    fn default() -> Self {
        Self {
            frames_per_callback: 160,
            callback_count: 4,
            start_value: 0.0,
            step_value: 1.0,
            interval: Duration::ZERO,
            start_delay: Duration::ZERO,
        }
    }
}

#[derive(Debug)]
pub enum SyntheticSourceError {
    InvalidFramesPerCallback,
    InvalidCallbackCount,
    AlreadyStarted,
    AlreadyStopped,
    ThreadPanic,
}

impl std::fmt::Display for SyntheticSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFramesPerCallback => {
                write!(f, "frames_per_callback must be greater than zero")
            }
            Self::InvalidCallbackCount => write!(f, "callback_count must be greater than zero"),
            Self::AlreadyStarted => write!(f, "synthetic source already started"),
            Self::AlreadyStopped => write!(f, "synthetic source already stopped"),
            Self::ThreadPanic => write!(f, "synthetic source thread panicked"),
        }
    }
}

impl std::error::Error for SyntheticSourceError {}

pub struct SyntheticSource {
    pipeline: Option<RealTimePipeline>,
    config: SyntheticSourceConfig,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SyntheticSource {
    pub fn new(
        pipeline: RealTimePipeline,
        config: SyntheticSourceConfig,
    ) -> Result<Self, SyntheticSourceError> {
        if config.frames_per_callback == 0 {
            return Err(SyntheticSourceError::InvalidFramesPerCallback);
        }

        if config.callback_count == 0 {
            return Err(SyntheticSourceError::InvalidCallbackCount);
        }

        Ok(Self {
            pipeline: Some(pipeline),
            config,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        })
    }

    pub fn start(&mut self) -> Result<(), SyntheticSourceError> {
        let Some(mut pipeline) = self.pipeline.take() else {
            return Err(SyntheticSourceError::AlreadyStarted);
        };

        let config = self.config;
        let stop = self.stop.clone();

        self.handle = Some(thread::spawn(move || {
            if !config.start_delay.is_zero() && !stop.load(Ordering::Relaxed) {
                thread::sleep(config.start_delay);
            }

            let channels = MASTER_FORMAT.channels as usize;
            let mut buffer = vec![0.0_f32; config.frames_per_callback * channels];
            let mut value = config.start_value;

            for _ in 0..config.callback_count {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                for frame in buffer.chunks_exact_mut(channels) {
                    for sample in frame {
                        *sample = value;
                    }
                    value += config.step_value;
                }

                pipeline.process_callback(&mut buffer);

                if !config.interval.is_zero() && !stop.load(Ordering::Relaxed) {
                    thread::sleep(config.interval);
                }
            }
        }));

        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), SyntheticSourceError> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return Err(SyntheticSourceError::AlreadyStopped);
        };

        if handle.join().is_err() {
            return Err(SyntheticSourceError::ThreadPanic);
        }

        Ok(())
    }
}

impl Drop for SyntheticSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AudioEngineController, SourceType};
    use ringbuf::traits::Consumer;
    use std::time::{Duration, Instant};

    #[test]
    fn new_rejects_zero_frames_per_callback() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let pipeline = engine
            .create_stream("s".to_string(), SourceType::Synthetic, 4, 4)
            .unwrap();
        let err = match SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 0,
                ..Default::default()
            },
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected InvalidFramesPerCallback"),
        };
        assert!(matches!(
            err,
            SyntheticSourceError::InvalidFramesPerCallback
        ));
        assert!(err.to_string().contains("frames_per_callback"));
    }

    #[test]
    fn new_rejects_zero_callback_count() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let pipeline = engine
            .create_stream("s".to_string(), SourceType::Synthetic, 4, 4)
            .unwrap();
        let err = match SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                callback_count: 0,
                ..Default::default()
            },
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected InvalidCallbackCount"),
        };
        assert!(matches!(err, SyntheticSourceError::InvalidCallbackCount));
        assert!(err.to_string().contains("callback_count"));
    }

    #[test]
    fn start_twice_returns_already_started() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let pipeline = engine
            .create_stream("s".to_string(), SourceType::Synthetic, 4, 4)
            .unwrap();
        let mut source = SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 4,
                callback_count: 1,
                start_delay: Duration::ZERO,
                ..Default::default()
            },
        )
        .unwrap();
        source.start().unwrap();
        let err = source.start().unwrap_err();
        assert!(matches!(err, SyntheticSourceError::AlreadyStarted));
        assert!(err.to_string().contains("already started"));
        source.stop().unwrap();
    }

    #[test]
    fn stop_without_start_returns_already_stopped() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let pipeline = engine
            .create_stream("s".to_string(), SourceType::Synthetic, 4, 4)
            .unwrap();
        let mut source = SyntheticSource::new(pipeline, Default::default()).unwrap();
        let err = source.stop().unwrap_err();
        assert!(matches!(err, SyntheticSourceError::AlreadyStopped));
        assert!(err.to_string().contains("already stopped"));
    }

    #[test]
    fn stop_twice_returns_already_stopped() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let pipeline = engine
            .create_stream("s".to_string(), SourceType::Synthetic, 4, 4)
            .unwrap();
        let mut source = SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 4,
                callback_count: 1,
                ..Default::default()
            },
        )
        .unwrap();
        source.start().unwrap();
        source.stop().unwrap();
        let err = source.stop().unwrap_err();
        assert!(matches!(err, SyntheticSourceError::AlreadyStopped));
    }

    #[test]
    fn error_display_covers_all_variants() {
        assert!(!format!("{}", SyntheticSourceError::InvalidFramesPerCallback).is_empty());
        assert!(!format!("{}", SyntheticSourceError::InvalidCallbackCount).is_empty());
        assert!(!format!("{}", SyntheticSourceError::AlreadyStarted).is_empty());
        assert!(!format!("{}", SyntheticSourceError::AlreadyStopped).is_empty());
        assert!(!format!("{}", SyntheticSourceError::ThreadPanic).is_empty());
    }

    #[test]
    fn stop_before_start_delay_emits_no_samples() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "synthetic".to_string();
        let route = "route".to_string();

        let pipeline = engine
            .create_stream(stream.clone(), SourceType::Synthetic, 8, 4)
            .expect("create stream");
        engine.route(&stream, &route).expect("route");

        let consumer = &mut engine
            .take_output_consumer(&route)
            .expect("output consumer present");
        let mut source = SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 4,
                callback_count: 4,
                start_delay: Duration::from_millis(30),
                ..Default::default()
            },
        )
        .expect("create synthetic source");

        source.start().expect("start synthetic source");
        source.stop().expect("stop synthetic source");

        assert!(consumer.try_pop().is_none());
    }

    #[test]
    fn zero_step_value_emits_constant_samples() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "synthetic".to_string();
        let route = "route".to_string();

        let pipeline = engine
            .create_stream(stream.clone(), SourceType::Synthetic, 8, 4)
            .expect("create stream");
        engine.route(&stream, &route).expect("route");

        let consumer = &mut engine
            .take_output_consumer(&route)
            .expect("output consumer present");
        let mut source = SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 2,
                callback_count: 2,
                start_value: 3.5,
                step_value: 0.0,
                ..Default::default()
            },
        )
        .expect("create synthetic source");

        source.start().expect("start synthetic source");

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut collected = Vec::new();
        while Instant::now() < deadline && collected.len() < 8 {
            if let Some(sample) = consumer.try_pop() {
                collected.push(sample);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }

        source.stop().expect("stop synthetic source");

        assert_eq!(collected, vec![3.5; 8]);
    }

    #[test]
    fn emits_predictable_stereo_values() {
        let mut engine = AudioEngineController::new(32, 32, 4096);
        let stream = "synthetic".to_string();
        let route = "route".to_string();

        let pipeline = engine
            .create_stream(stream.clone(), SourceType::Synthetic, 8, 4)
            .expect("create stream");
        engine.route(&stream, &route).expect("route");

        let consumer = &mut engine
            .take_output_consumer(&route)
            .expect("output consumer present");
        let mut source = SyntheticSource::new(
            pipeline,
            SyntheticSourceConfig {
                frames_per_callback: 4,
                callback_count: 2,
                start_value: 1.0,
                step_value: 1.0,
                interval: Duration::ZERO,
                start_delay: Duration::from_millis(10),
            },
        )
        .expect("create synthetic source");

        source.start().expect("start synthetic source");

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut collected = Vec::new();
        while Instant::now() < deadline && collected.len() < 16 {
            if let Some(sample) = consumer.try_pop() {
                collected.push(sample);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }

        source.stop().expect("stop synthetic source");

        assert_eq!(
            collected,
            vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0, 5.0, 5.0, 6.0, 6.0, 7.0, 7.0, 8.0, 8.0,]
        );
    }
}
