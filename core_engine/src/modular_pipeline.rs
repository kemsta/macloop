use crate::config::AudioProcessingConfig;
use crate::delay_measurement::DelayTracker;
use crate::messages::{AudioFrame, AudioSourceType};
use crate::processors::{
    AecProcessor, AudioProcessor, FrameQuantizer, NoiseSuppressionProcessor, ResampleProcessor,
    TimestampNormalizer,
};
use crate::stats::RuntimeStatsHandle;
use crossbeam_channel::Receiver;
use std::time::Instant;

pub struct ModularPipeline {
    rx: Receiver<AudioFrame>,
    stop_rx: Receiver<()>,
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
        config: AudioProcessingConfig,
        stats: RuntimeStatsHandle,
    ) -> Self {
        let mut processors: Vec<Box<dyn AudioProcessor>> = Vec::new();

        processors.push(Box::new(TimestampNormalizer::new()));

        if config.enable_aec || config.enable_ns {
            processors.push(Box::new(ResampleProcessor::new(
                48_000,
                48_000,
                1,
                AudioSourceType::System,
            )));
            processors.push(Box::new(FrameQuantizer::for_webrtc()));
        }

        if config.enable_aec {
            processors.push(Box::new(AecProcessor::new(config.clone(), stats.clone())));
        }

        if config.enable_ns {
            processors.push(Box::new(NoiseSuppressionProcessor::new(config.clone())));
        }

        Self {
            rx,
            stop_rx,
            processors,
            config,
            delay_tracker: DelayTracker::new(),
            stats,
        }
    }

    pub fn create_system_pipeline(config: &AudioProcessingConfig) -> Vec<Box<dyn AudioProcessor>> {
        vec![Box::new(ResampleProcessor::from_config(
            config,
            AudioSourceType::System,
        ))]
    }

    pub fn create_mic_pipeline(config: &AudioProcessingConfig) -> Vec<Box<dyn AudioProcessor>> {
        vec![Box::new(ResampleProcessor::from_config(
            config,
            AudioSourceType::Microphone,
        ))]
    }

    pub fn run_with_handler<F>(&mut self, mut on_frame: F)
    where
        F: FnMut(AudioFrame),
    {
        let config = self.config.clone();
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

            match frame.source {
                AudioSourceType::System => {
                    self.stats.update(|s| s.frames_in_system += 1);

                    if self.config.enable_aec {
                        let _ = self.process_through_pipeline(frame.clone());
                    }

                    let final_frames =
                        Self::process_through_processors_static(&mut sys_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.stats.update(|s| s.frames_out_system += 1);
                        on_frame(final_frame);
                    }
                }
                AudioSourceType::Microphone => {
                    self.stats.update(|s| s.frames_in_mic += 1);
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

                        for processed_frame in processed_frames {
                            let final_frames = Self::process_through_processors_static(
                                &mut mic_pipeline,
                                processed_frame,
                                &self.stats,
                            );
                            for final_frame in final_frames {
                                self.stats.update(|s| s.frames_out_mic += 1);
                                on_frame(final_frame);
                            }
                        }
                    }
                }
            }
        }

        self.flush_all_processors(&mut sys_pipeline, &mut mic_pipeline, &mut on_frame);
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
                        let _ = processed.timestamp.saturating_sub(input_timestamp);

                        if let Some(stage) = Self::stage_key(
                            i,
                            has_webrtc,
                            self.config.enable_aec,
                            self.config.enable_ns,
                        ) {
                            self.delay_tracker.record(stage, processing_time);
                            self.stats.update(|s| match stage {
                                "timestamp_processor" => s.timestamp_processor.record(processing_time),
                                "webrtc_resample_processor" => {
                                    s.webrtc_resample_processor.record(processing_time)
                                }
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

    fn flush_all_processors<F>(
        &mut self,
        sys_pipeline: &mut [Box<dyn AudioProcessor>],
        mic_pipeline: &mut [Box<dyn AudioProcessor>],
        on_frame: &mut F,
    ) where
        F: FnMut(AudioFrame),
    {
        let mut all_frames = Vec::new();
        for processor in &mut self.processors {
            all_frames.extend(processor.flush());
        }

        for frame in all_frames {
            match frame.source {
                AudioSourceType::System => {
                    let final_frames =
                        Self::process_through_processors_static(sys_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.stats.update(|s| s.frames_out_system += 1);
                        on_frame(final_frame);
                    }
                }
                AudioSourceType::Microphone => {
                    let final_frames =
                        Self::process_through_processors_static(mic_pipeline, frame, &self.stats);
                    for final_frame in final_frames {
                        self.stats.update(|s| s.frames_out_mic += 1);
                        on_frame(final_frame);
                    }
                }
            }
        }

        for processor in sys_pipeline {
            for frame in processor.flush() {
                self.stats.update(|s| s.frames_out_system += 1);
                on_frame(frame);
            }
        }

        for processor in mic_pipeline {
            for frame in processor.flush() {
                self.stats.update(|s| s.frames_out_mic += 1);
                on_frame(frame);
            }
        }
    }
}
