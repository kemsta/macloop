use webrtc_audio_processing::{Config, Processor};
use webrtc_audio_processing::config::{EchoCanceller, HighPassFilter};
use crate::config::AudioProcessingConfig;
use crate::messages::{AudioFrame, AudioSourceType};
use crate::stats::RuntimeStatsHandle;
use super::AudioProcessor;
use anyhow::Result;

/// AEC Processor for WebRTC stream flow:
/// - render/system frames are fed immediately via process_render_frame
/// - capture/mic frames are fed immediately via process_capture_frame
pub struct AecProcessor {
    apm: Option<Processor>,
    config: AudioProcessingConfig,

    applied_delay_ms: i32,
    tuner_last_erle: Option<f64>,
    tuner_erle_ema: Option<f64>,
    tuner_best_erle: Option<f64>,
    tuner_best_delay_ms: i32,
    tuner_direction: i32,
    tuner_step_ms: i32,
    tuner_interval_frames: u64,
    tuner_max_delay_ms: i32,
    tuner_frozen: bool,
    tuner_stable_windows: u32,

    last_system_active_frame: Option<u64>,
    sys_frames_seen: u64,
    mic_frames_seen: u64,
    skipped_inactive_mic: u64,
    skipped_inactive_system: u64,
    stats: RuntimeStatsHandle,
}

impl AecProcessor {
    const SIGNAL_ACTIVITY_THRESHOLD: f32 = 1.0e-4;
    const SYSTEM_ACTIVITY_GRACE_FRAMES: u64 = 30;

    pub fn new(config: AudioProcessingConfig, stats: RuntimeStatsHandle) -> Self {
        let apm = if config.enable_aec {
            Self::create_apm(&config)
        } else {
            None
        };
        let initial_delay_ms = config.aec_stream_delay_ms.max(0);
        let tuner_max_delay_ms = config.aec_max_delay_ms.clamp(20, 1000);

        let processor = Self {
            apm,
            config,
            applied_delay_ms: initial_delay_ms,
            tuner_last_erle: None,
            tuner_erle_ema: None,
            tuner_best_erle: None,
            tuner_best_delay_ms: initial_delay_ms,
            tuner_direction: 1,
            tuner_step_ms: 5,
            tuner_interval_frames: 50,
            tuner_max_delay_ms,
            tuner_frozen: false,
            tuner_stable_windows: 0,
            last_system_active_frame: None,
            sys_frames_seen: 0,
            mic_frames_seen: 0,
            skipped_inactive_mic: 0,
            skipped_inactive_system: 0,
            stats,
        };
        processor.publish_tuner_stats(None, None);
        processor
    }

    pub fn process_frame(&mut self, mut frame: AudioFrame) -> Result<Option<AudioFrame>> {
        match frame.source {
            AudioSourceType::System => {
                self.sys_frames_seen += 1;
                if Self::frame_is_active(&frame.samples) {
                    self.last_system_active_frame = Some(self.sys_frames_seen);
                }

                if let Some(apm) = &mut self.apm {
                    if let Err(e) = apm.process_render_frame([frame.samples.as_mut_slice()]) {
                        eprintln!("Critical APM Render Error: {}", e);
                    }
                }

                Ok(None)
            }
            AudioSourceType::Microphone => {
                self.mic_frames_seen += 1;
                let mic_active = Self::frame_is_active(&frame.samples);
                let sys_active_recently = self
                    .last_system_active_frame
                    .map(|last| self.sys_frames_seen.saturating_sub(last) <= Self::SYSTEM_ACTIVITY_GRACE_FRAMES)
                    .unwrap_or(false);

                let should_tune = self.config.aec_auto_delay_tuning
                    && !self.tuner_frozen
                    && mic_active
                    && sys_active_recently
                    && self.mic_frames_seen % self.tuner_interval_frames == 0;
                let tune_tick = self.config.aec_auto_delay_tuning
                    && !self.tuner_frozen
                    && self.mic_frames_seen % self.tuner_interval_frames == 0;
                if tune_tick {
                    if !mic_active {
                        self.skipped_inactive_mic += 1;
                    }
                    if !sys_active_recently {
                        self.skipped_inactive_system += 1;
                    }
                }
                let mut erle_snapshot: Option<f64> = None;
                let mut delay_snapshot: Option<u32> = None;
                if let Some(apm) = &mut self.apm {
                    if let Err(e) = apm.process_capture_frame([frame.samples.as_mut_slice()]) {
                        eprintln!("Critical APM Capture Error: {}", e);
                    }
                    if should_tune {
                        let stats = apm.get_stats();
                        erle_snapshot = stats.echo_return_loss_enhancement;
                        delay_snapshot = stats.delay_ms;
                    }
                }
                if should_tune {
                    if let Some(erle) = erle_snapshot {
                        let tuned = self.tune_delay_on_the_fly(erle, delay_snapshot);
                        if tuned {
                            if let Some(apm) = &self.apm {
                                apm.set_config(Self::build_apm_config(self.applied_delay_ms));
                            }
                        }
                    }
                }
                self.publish_tuner_stats(erle_snapshot, delay_snapshot);

                Ok(Some(frame))
            }
        }
    }

    fn frame_is_active(samples: &[f32]) -> bool {
        samples
            .iter()
            .any(|s| s.abs() >= Self::SIGNAL_ACTIVITY_THRESHOLD)
    }

    fn publish_tuner_stats(&self, erle_snapshot: Option<f64>, delay_snapshot: Option<u32>) {
        let enabled = self.config.aec_auto_delay_tuning;
        let frozen = self.tuner_frozen;
        let applied_delay_ms = self.applied_delay_ms;
        let best_delay_ms = self.tuner_best_delay_ms;
        let step_ms = self.tuner_step_ms;
        let direction = self.tuner_direction;
        let interval_frames = self.tuner_interval_frames;
        let max_delay_ms = self.tuner_max_delay_ms;
        let last_erle = erle_snapshot.or(self.tuner_last_erle);
        let erle_ema = self.tuner_erle_ema;
        let best_erle = self.tuner_best_erle;
        let skipped_inactive_mic = self.skipped_inactive_mic;
        let skipped_inactive_system = self.skipped_inactive_system;

        self.stats.update(|s| {
            s.aec_tuner.enabled = enabled;
            s.aec_tuner.frozen = frozen;
            s.aec_tuner.applied_delay_ms = applied_delay_ms;
            s.aec_tuner.best_delay_ms = best_delay_ms;
            s.aec_tuner.step_ms = step_ms;
            s.aec_tuner.direction = direction;
            s.aec_tuner.interval_frames = interval_frames;
            s.aec_tuner.max_delay_ms = max_delay_ms;
            s.aec_tuner.last_erle = last_erle;
            s.aec_tuner.erle_ema = erle_ema;
            s.aec_tuner.best_erle = best_erle;
            s.aec_tuner.last_apm_delay_ms = delay_snapshot;
            s.aec_tuner.skipped_inactive_mic = skipped_inactive_mic;
            s.aec_tuner.skipped_inactive_system = skipped_inactive_system;
        });
    }

    fn tune_delay_on_the_fly(&mut self, erle: f64, delay_inst_ms: Option<u32>) -> bool {
        // Ignore unstable snapshots where internal delay estimate spikes unnaturally.
        if let Some(d) = delay_inst_ms {
            if d >= 250 && erle < 1.0 {
                return false;
            }
        }

        let ema = if let Some(prev) = self.tuner_erle_ema {
            prev * 0.7 + erle * 0.3
        } else {
            erle
        };
        self.tuner_erle_ema = Some(ema);

        if self.tuner_best_erle.map(|best| ema > best + 0.1).unwrap_or(true) {
            self.tuner_best_erle = Some(ema);
            self.tuner_best_delay_ms = self.applied_delay_ms;
            self.tuner_stable_windows = 0;
        }

        // Auto-freeze when ERLE is high and stable.
        if let Some(best) = self.tuner_best_erle {
            if best >= 3.5 && (ema - best).abs() <= 0.1 {
                self.tuner_stable_windows += 1;
            } else {
                self.tuner_stable_windows = 0;
            }

            if self.tuner_stable_windows >= 8 {
                self.applied_delay_ms = self.tuner_best_delay_ms;
                self.tuner_frozen = true;
                self.stats.update(|s| s.aec_tuner.freeze_events += 1);
                return true;
            }
        }

        // If quality dropped a lot, snap back to best-known delay.
        if let Some(best) = self.tuner_best_erle {
            if ema < best - 1.0 && self.applied_delay_ms != self.tuner_best_delay_ms {
                self.applied_delay_ms = self.tuner_best_delay_ms;
                self.tuner_direction = -self.tuner_direction;
                self.tuner_step_ms = (self.tuner_step_ms - 1).max(2);
                self.stats.update(|s| s.aec_tuner.rollback_events += 1);
                return true;
            }
        }

        if let Some(last_erle) = self.tuner_last_erle {
            if erle + 0.2 < last_erle {
                // Quality worsened: reverse direction and shrink step.
                self.tuner_direction = -self.tuner_direction;
                self.tuner_step_ms = (self.tuner_step_ms - 1).max(2);
            } else if erle > last_erle + 0.2 {
                // Quality improved: allow slightly larger step.
                self.tuner_step_ms = (self.tuner_step_ms + 1).min(8);
            }
        }

        let min_delay = (self.tuner_best_delay_ms - 40).max(0);
        let max_delay = (self.tuner_best_delay_ms + 40).min(self.tuner_max_delay_ms);
        let next_delay = (self.applied_delay_ms + self.tuner_direction * self.tuner_step_ms)
            .clamp(min_delay, max_delay);
        self.tuner_last_erle = Some(erle);

        if next_delay == self.applied_delay_ms {
            self.tuner_direction = -self.tuner_direction;
            return false;
        }

        self.applied_delay_ms = next_delay;
        self.stats.update(|s| s.aec_tuner.tune_events += 1);
        true
    }

    fn build_apm_config(delay_ms: i32) -> Config {
        let mut apm_config = Config::default();
        apm_config.high_pass_filter = Some(HighPassFilter::default());
        apm_config.echo_canceller = Some(EchoCanceller::Full {
            stream_delay_ms: if delay_ms > 0 { Some(delay_ms as u16) } else { None },
        });
        apm_config.noise_suppression = None;
        apm_config.gain_controller = None;
        apm_config
    }

    fn create_apm(config: &AudioProcessingConfig) -> Option<Processor> {
        let apm = match Processor::new(48_000) {
            Ok(apm) => apm,
            Err(err) => {
                eprintln!("Warning: failed to create AEC processor: {}", err);
                return None;
            }
        };
        let delay_ms = config.aec_stream_delay_ms.max(0);
        apm.set_config(Self::build_apm_config(delay_ms));
        Some(apm)
    }
}

impl AudioProcessor for AecProcessor {
    fn process(&mut self, frame: AudioFrame) -> Result<Option<AudioFrame>> {
        self.process_frame(frame)
    }

    fn flush(&mut self) -> Vec<AudioFrame> {
        Vec::new()
    }

    fn reset(&mut self) {
        self.applied_delay_ms = self.config.aec_stream_delay_ms.max(0);
        self.tuner_last_erle = None;
        self.tuner_erle_ema = None;
        self.tuner_best_erle = None;
        self.tuner_best_delay_ms = self.applied_delay_ms;
        self.tuner_direction = 1;
        self.tuner_step_ms = 5;
        self.tuner_frozen = false;
        self.tuner_stable_windows = 0;
        self.last_system_active_frame = None;
        self.sys_frames_seen = 0;
        self.mic_frames_seen = 0;
        self.skipped_inactive_mic = 0;
        self.skipped_inactive_system = 0;
        self.publish_tuner_stats(None, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(enable_aec: bool, auto_tune: bool) -> AudioProcessingConfig {
        AudioProcessingConfig {
            sample_rate: 16_000,
            channels: 1,
            enable_aec,
            enable_ns: false,
            sample_format: "f32".to_string(),
            aec_stream_delay_ms: 10,
            aec_auto_delay_tuning: auto_tune,
            aec_max_delay_ms: 140,
        }
    }

    fn frame(source: AudioSourceType, amp: f32) -> AudioFrame {
        AudioFrame {
            source,
            samples: vec![amp; 480],
            sample_rate: 48_000,
            channels: 1,
            timestamp: 0,
        }
    }

    #[test]
    fn inactive_streams_increment_skip_counters() {
        let stats = RuntimeStatsHandle::new();
        let mut aec = AecProcessor::new(config(true, true), stats.clone());

        for _ in 0..50 {
            let _ = aec.process(frame(AudioSourceType::Microphone, 0.0)).unwrap();
        }

        let snap = stats.snapshot();
        assert!(snap.aec_tuner.skipped_inactive_mic >= 1);
        assert!(snap.aec_tuner.skipped_inactive_system >= 1);
    }

    #[test]
    fn tune_updates_delay_and_stats() {
        let stats = RuntimeStatsHandle::new();
        let mut aec = AecProcessor::new(config(false, true), stats.clone());
        let before = aec.applied_delay_ms;
        let tuned = aec.tune_delay_on_the_fly(2.0, Some(10));

        assert!(tuned);
        assert_ne!(aec.applied_delay_ms, before);
        assert_eq!(stats.snapshot().aec_tuner.tune_events, 1);
    }

    #[test]
    fn reset_restores_tuner_baseline() {
        let stats = RuntimeStatsHandle::new();
        let mut aec = AecProcessor::new(config(false, true), stats.clone());
        let _ = aec.tune_delay_on_the_fly(3.0, Some(10));
        aec.reset();

        assert_eq!(aec.applied_delay_ms, 10);
        assert_eq!(aec.tuner_step_ms, 5);
        assert!(!aec.tuner_frozen);
        let snap = stats.snapshot();
        assert_eq!(snap.aec_tuner.applied_delay_ms, 10);
    }

    #[test]
    fn system_frames_are_consumed_and_not_forwarded() {
        let stats = RuntimeStatsHandle::new();
        let mut aec = AecProcessor::new(config(false, false), stats);
        let out = aec.process(frame(AudioSourceType::System, 0.5)).unwrap();
        assert!(out.is_none());
    }
}
