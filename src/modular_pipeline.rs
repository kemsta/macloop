use crossbeam_channel::Receiver;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use crate::messages::AudioFrame;
use crate::config::AudioProcessingConfig;
use crate::processors::{AudioProcessor, TimestampNormalizer, ResampleProcessor, AecProcessor, NoiseSuppressionProcessor, FrameQuantizer};
use crate::delay_measurement::DelayTracker;
use crate::stats::RuntimeStatsHandle;
use numpy::ToPyArray;
use std::time::Instant;

/// Modular pipeline that processes audio through a chain of processors
pub struct ModularPipeline {
    rx: Receiver<AudioFrame>,
    stop_rx: Receiver<()>,
    callback: Py<PyAny>,
    processors: Vec<Box<dyn AudioProcessor>>,
    config: AudioProcessingConfig,
    delay_tracker: DelayTracker,
    stats: RuntimeStatsHandle,
}

impl ModularPipeline {
    fn select_total_pipeline_delay(input_timestamp: u64, output_timestamp: u64, processing_delay: u64) -> u64 {
        let timestamp_delay = output_timestamp.saturating_sub(input_timestamp);
        if timestamp_delay > 0 && timestamp_delay < 10_000_000_000 {
            timestamp_delay
        } else {
            processing_delay
        }
    }

    fn stage_key(index: usize, has_webrtc: bool, enable_aec: bool, enable_ns: bool) -> Option<&'static str> {
        match index {
            0 => Some("timestamp_processor"),
            1 if has_webrtc => Some("webrtc_resample_processor"),
            2 if has_webrtc => Some("quantizer_processor"),
            3 if enable_aec && has_webrtc => Some("aec_processor"),
            4 if enable_ns && has_webrtc => Some("ns_processor"),
            _ => None,
        }
    }

    pub fn new(
        rx: Receiver<AudioFrame>, 
        stop_rx: Receiver<()>,
        callback: Py<PyAny>, 
        config: AudioProcessingConfig,
        stats: RuntimeStatsHandle,
    ) -> Self {
        let mut processors: Vec<Box<dyn AudioProcessor>> = Vec::new();
        
        // Stage 1: Timestamp normalization
        processors.push(Box::new(TimestampNormalizer::new()));
        
        // Stage 2: Resample to 48kHz mono for WebRTC processing (if needed)
        if config.enable_aec || config.enable_ns {
            processors.push(Box::new(ResampleProcessor::new(
                48000, 48000, 1, // 48kHz stereo/mono -> 48kHz mono
                crate::messages::AudioSourceType::System // Will be overridden per frame
            )));
        }
        
        // Stage 3: Frame Quantization for WebRTC (if any WebRTC feature is enabled)
        if config.enable_aec || config.enable_ns {
            processors.push(Box::new(FrameQuantizer::for_webrtc()));
        }
        
        // Stage 4: AEC Processing (if enabled)
        if config.enable_aec {
            processors.push(Box::new(AecProcessor::new(config.clone(), stats.clone())));
        }
        
        // Stage 5: Noise Suppression (if enabled)
        if config.enable_ns {
            processors.push(Box::new(NoiseSuppressionProcessor::new(config.clone())));
        }
        
        // Stage 3: Resampling for system audio (48kHz -> target rate)
        // Note: We'll need separate pipelines for mic and system due to different target formats
        
        Self {
            rx,
            stop_rx,
            callback,
            processors,
            config,
            delay_tracker: DelayTracker::new(),
            stats,
        }
    }
    
    /// Create processing pipeline for system audio (direct, no WebRTC processing)
    pub fn create_system_pipeline(config: &AudioProcessingConfig) -> Vec<Box<dyn AudioProcessor>> {
        let mut processors: Vec<Box<dyn AudioProcessor>> = Vec::new();
        
        // System audio: direct 48kHz stereo -> target rate/channels
        // No WebRTC processing - just final resampling/channel conversion
        processors.push(Box::new(ResampleProcessor::from_config(
            config, 
            crate::messages::AudioSourceType::System
        )));
        
        processors
    }
    
    /// Create processing pipeline for microphone audio (after WebRTC processing)
    pub fn create_mic_pipeline(config: &AudioProcessingConfig) -> Vec<Box<dyn AudioProcessor>> {
        let mut processors: Vec<Box<dyn AudioProcessor>> = Vec::new();
        
        // Microphone: WebRTC-processed 48kHz mono -> target rate/channels
        processors.push(Box::new(ResampleProcessor::from_config(
            config, 
            crate::messages::AudioSourceType::Microphone
        )));
        
        processors
    }
    
    pub fn run(&mut self) {
        let config = self.config.clone();
        let callback = Python::attach(|py| self.callback.clone_ref(py));
        
        // Create separate pipelines for final processing after AEC
        let mut sys_pipeline = Self::create_system_pipeline(&config);
        let mut mic_pipeline = Self::create_mic_pipeline(&config);
        
        loop {
            let frame = crossbeam_channel::select! {
                recv(self.stop_rx) -> _ => break,
                recv(self.rx) -> msg => match msg {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
            };
            // Split processing based on source type
            match frame.source {
                crate::messages::AudioSourceType::System => {
                    self.stats.update(|s| s.frames_in_system += 1);
                    // System audio: direct to final resampling (but also feed to AEC as reference)
                    
                    // 1. Send copy to AEC for reference processing
                    if self.config.enable_aec {
                        let aec_frame = frame.clone();
                        let _ = self.process_through_pipeline(aec_frame);
                    }
                    
                    // 2. Process original system frame through final pipeline
                    let final_frames = Self::process_through_processors_static(&mut sys_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.send_frame_to_python("system", final_frame, &config, &callback);
                    }
                }
                crate::messages::AudioSourceType::Microphone => {
                    self.stats.update(|s| s.frames_in_mic += 1);
                    // Microphone: full processing pipeline
                    let pipeline_start = Instant::now();
                    let input_timestamp = frame.timestamp;
                    
                    let processed_frames = self.process_through_pipeline(frame);
                    if !processed_frames.is_empty() {
                        let processing_delay = pipeline_start.elapsed().as_nanos() as u64;
                        self.delay_tracker.record("processing_time", processing_delay);
                        self.stats.update(|s| s.processing_time.record(processing_delay));
                        
                        let total_delay = Self::select_total_pipeline_delay(
                            input_timestamp,
                            processed_frames[0].timestamp,
                            processing_delay,
                        );
                        self.delay_tracker.record("total_pipeline", total_delay);
                        self.stats.update(|s| s.total_pipeline.record(total_delay));
                        
                        // Send all processed mic frames to final pipeline
                        for processed_frame in processed_frames {
                            let final_frames = Self::process_through_processors_static(&mut mic_pipeline, processed_frame, &self.stats);
                            for final_frame in final_frames {
                                self.send_frame_to_python("mic", final_frame, &config, &callback);
                            }
                        }
                    }
                }
            }
        }
        
        // Flush all processors
        self.flush_all_processors(&mut sys_pipeline, &mut mic_pipeline, &config, &callback);
    }
    
    fn process_through_pipeline(&mut self, frame: AudioFrame) -> Vec<AudioFrame> {
        let mut frames = vec![frame];

        for (i, processor) in self.processors.iter_mut().enumerate() {
            let start_time = Instant::now();
            let has_webrtc = self.config.enable_aec || self.config.enable_ns;
            let mut next_frames = Vec::new();

            for input in frames {
                let input_timestamp = input.timestamp;
                match processor.process(input) {
                    Ok(Some(processed)) => {
                        let processing_time = start_time.elapsed().as_nanos() as u64;
                        let _timestamp_diff = processed.timestamp.saturating_sub(input_timestamp);

                        if let Some(stage) = Self::stage_key(
                            i,
                            has_webrtc,
                            self.config.enable_aec,
                            self.config.enable_ns,
                        ) {
                            self.delay_tracker.record(stage, processing_time);
                            self.stats.update(|s| match stage {
                                "timestamp_processor" => s.timestamp_processor.record(processing_time),
                                "webrtc_resample_processor" => s.webrtc_resample_processor.record(processing_time),
                                "quantizer_processor" => s.quantizer_processor.record(processing_time),
                                "aec_processor" => s.aec_processor.record(processing_time),
                                "ns_processor" => s.ns_processor.record(processing_time),
                                _ => {}
                            });
                        }

                        next_frames.push(processed);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.stats.update(|s| s.processor_errors += 1);
                        eprintln!("Warning: Processor error: {}", e);
                    }
                }
            }

            loop {
                match processor.drain_ready() {
                    Ok(Some(ready)) => next_frames.push(ready),
                    Ok(None) => break,
                    Err(e) => {
                        self.stats.update(|s| s.processor_drain_errors += 1);
                        eprintln!("Warning: Processor drain error: {}", e);
                        break;
                    }
                }
            }

            if next_frames.is_empty() {
                return Vec::new();
            }

            frames = next_frames;
        }

        frames
    }
    
    fn process_through_processors_static(
        processors: &mut [Box<dyn AudioProcessor>], 
        frame: AudioFrame,
        stats: &RuntimeStatsHandle,
    ) -> Vec<AudioFrame> {
        let mut frames = vec![frame];

        for processor in processors {
            let mut next_frames = Vec::new();
            for input in frames {
                match processor.process(input) {
                    Ok(Some(processed)) => next_frames.push(processed),
                    Ok(None) => {}
                    Err(e) => {
                        stats.update(|s| s.processor_errors += 1);
                        eprintln!("Warning: Final processor error: {}", e);
                    }
                }
            }

            loop {
                match processor.drain_ready() {
                    Ok(Some(ready)) => next_frames.push(ready),
                    Ok(None) => break,
                    Err(e) => {
                        stats.update(|s| s.processor_drain_errors += 1);
                        eprintln!("Warning: Final processor drain error: {}", e);
                        break;
                    }
                }
            }

            if next_frames.is_empty() {
                return Vec::new();
            }

            frames = next_frames;
        }

        frames
    }
    
    fn flush_all_processors(
        &mut self,
        sys_pipeline: &mut [Box<dyn AudioProcessor>],
        mic_pipeline: &mut [Box<dyn AudioProcessor>],
        config: &AudioProcessingConfig,
        callback: &Py<PyAny>
    ) {
        // Flush main pipeline
        let mut all_frames = Vec::new();
        for processor in &mut self.processors {
            let frames = processor.flush();
            all_frames.extend(frames);
        }
        
        // Process flushed frames
        for frame in all_frames {
            match frame.source {
                crate::messages::AudioSourceType::System => {
                    let final_frames = Self::process_through_processors_static(sys_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.send_frame_to_python("system", final_frame, config, callback);
                    }
                }
                crate::messages::AudioSourceType::Microphone => {
                    let final_frames = Self::process_through_processors_static(mic_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.send_frame_to_python("mic", final_frame, config, callback);
                    }
                }
            }
        }
        
        // Flush final pipelines
        for processor in sys_pipeline {
            let frames = processor.flush();
            for frame in frames {
                self.send_frame_to_python("system", frame, config, callback);
            }
        }
        
        for processor in mic_pipeline {
            let frames = processor.flush();
            for frame in frames {
                self.send_frame_to_python("mic", frame, config, callback);
            }
        }
    }
    
    fn send_frame_to_python(
        &self,
        source_name: &str,
        frame: AudioFrame,
        config: &AudioProcessingConfig,
        callback: &Py<PyAny>
    ) {
        if let Some(_) = Python::try_attach(|py| {
            match (|| -> pyo3::PyResult<()> {
                let frame_np = Self::to_numpy(py, &frame.samples, &config.sample_format);
                callback.call1(py, (source_name, frame_np))?;
                Ok(())
            })() {
                Ok(_) => {
                    self.stats.update(|s| {
                        if source_name == "mic" {
                            s.frames_out_mic += 1;
                        } else if source_name == "system" {
                            s.frames_out_system += 1;
                        }
                    });
                },
                Err(e) => {
                    self.stats.update(|s| s.callback_errors += 1);
                    eprintln!("Warning: Python callback error: {}", e);
                }
            }
        }) {
            // Success
        } else {
            self.stats.update(|s| s.gil_acquire_failures += 1);
            eprintln!("Warning: Could not acquire Python GIL");
        }
    }
    
fn to_numpy<'py>(py: Python<'py>, samples: &[f32], format: &str) -> Py<PyAny> {
        if format == "i16" {
            let i16_samples = samples.iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            numpy::PyArray1::from_iter(py, i16_samples).into_any().unbind()
        } else {
            samples.to_pyarray(py).into_any().unbind()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::AudioSourceType;

    struct DropProcessor;
    impl AudioProcessor for DropProcessor {
        fn process(&mut self, _frame: AudioFrame) -> anyhow::Result<Option<AudioFrame>> { Ok(None) }
        fn flush(&mut self) -> Vec<AudioFrame> { Vec::new() }
        fn reset(&mut self) {}
    }

    #[test]
    fn total_pipeline_delay_prefers_timestamp_when_plausible() {
        let d = ModularPipeline::select_total_pipeline_delay(1_000, 2_000, 777);
        assert_eq!(d, 1_000);
    }

    #[test]
    fn total_pipeline_delay_falls_back_on_implausible_values() {
        let d_zero = ModularPipeline::select_total_pipeline_delay(1_000, 1_000, 777);
        let d_huge = ModularPipeline::select_total_pipeline_delay(0, 11_000_000_000, 888);
        assert_eq!(d_zero, 777);
        assert_eq!(d_huge, 888);
    }

    #[test]
    fn stage_mapping_matches_configuration() {
        assert_eq!(ModularPipeline::stage_key(0, false, false, false), Some("timestamp_processor"));
        assert_eq!(ModularPipeline::stage_key(1, true, false, false), Some("webrtc_resample_processor"));
        assert_eq!(ModularPipeline::stage_key(2, true, false, false), Some("quantizer_processor"));
        assert_eq!(ModularPipeline::stage_key(3, true, true, false), Some("aec_processor"));
        assert_eq!(ModularPipeline::stage_key(4, true, true, true), Some("ns_processor"));
        assert_eq!(ModularPipeline::stage_key(3, true, false, true), None);
    }

    #[test]
    fn static_processor_pipeline_stops_when_frame_dropped() {
        let mut processors: Vec<Box<dyn AudioProcessor>> = vec![Box::new(DropProcessor)];
        let stats = RuntimeStatsHandle::new();
        let input = AudioFrame {
            source: AudioSourceType::Microphone,
            samples: vec![0.1; 10],
            sample_rate: 48_000,
            channels: 1,
            timestamp: 0,
        };
        let out = ModularPipeline::process_through_processors_static(&mut processors, input, &stats);
        assert!(out.is_empty());
    }
}
