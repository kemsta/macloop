use crossbeam_channel::Sender;
use crate::messages::{AudioFrame, AudioSourceType};
use crate::config::AudioProcessingConfig;
use screencapturekit::prelude::*;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use anyhow::{Result, anyhow};

#[derive(Clone, Copy)]
pub enum CaptureTarget {
    Display(u32),
    Process(i32),
}

struct AudioOutputHandler {
    tx: Sender<AudioFrame>,
    source_type: AudioSourceType,
}

fn cm_time_to_ns(value: i64, timescale: i32) -> u64 {
    if timescale <= 0 || value < 0 {
        return 0;
    }

    let value = value as u64;
    let timescale = timescale as u64;

    if value <= u64::MAX / 1_000_000_000 {
        (value * 1_000_000_000) / timescale
    } else {
        // For very large values, divide first to avoid multiplication overflow.
        value / timescale * 1_000_000_000 + (value % timescale * 1_000_000_000) / timescale
    }
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        let is_target = match (self.source_type, of_type) {
            (AudioSourceType::System, SCStreamOutputType::Audio) => true,
            (AudioSourceType::Microphone, SCStreamOutputType::Microphone) => true,
            _ => false,
        };

        if is_target {
            if let Some(audio_data) = sample_buffer.audio_buffer_list() {
                let mut interleaved_samples = Vec::new();
                let num_buffers = audio_data.num_buffers();
                
                if num_buffers == 0 { return; }

                // Get first buffer to check channels; skip malformed empty payloads.
                let Some(first_buffer) = audio_data.get(0) else { return; };
                let channels_per_buffer = first_buffer.number_channels as usize;

                if num_buffers == 1 {
                    // Already interleaved or mono
                    let samples: &[f32] = bytemuck::cast_slice(first_buffer.data());
                    interleaved_samples.extend_from_slice(samples);
                } else {
                    // Planar data: Buffers [L, R]. We need to interleave them.
                    let bufs: Vec<&[f32]> = audio_data.iter()
                        .map(|b| bytemuck::cast_slice::<u8, f32>(b.data()))
                        .collect();
                    
                    let frames = bufs[0].len();
                    interleaved_samples.reserve(frames * num_buffers);
                    
                    for i in 0..frames {
                        for b in &bufs {
                            if i < b.len() {
                                interleaved_samples.push(b[i]);
                            }
                        }
                    }
                }

                if !interleaved_samples.is_empty() {
                    let pts = sample_buffer.presentation_timestamp();
                    let timestamp = cm_time_to_ns(pts.value, pts.timescale);

                    let packet = AudioFrame {
                        source: self.source_type,
                        samples: interleaved_samples,
                        sample_rate: 48000, 
                        channels: (num_buffers * channels_per_buffer) as u16,
                        timestamp,
                    };
                    // Use send() to ensure delivery. Unbounded channel prevents blocking.
                    let _ = self.tx.send(packet);
                }
            }
        }
    }
}

pub fn spawn_capture_engine(
    tx: Sender<AudioFrame>, 
    target: Option<CaptureTarget>, 
    _config: AudioProcessingConfig,
    capture_system: bool,
    capture_mic: bool
) -> Result<SCStream> {
    let content = SCShareableContent::get().map_err(|e| anyhow!("Failed to get shareable content: {}", e))?;
    // Even if we only capture mic, SCK needs a filter. We'll use the first display as a base.
    let filter = match target {
        Some(CaptureTarget::Display(display_id)) => {
            let display = content.displays().into_iter()
                .find(|d| d.display_id() == display_id)
                .ok_or_else(|| anyhow!("Display with ID {} not found", display_id))?;
            SCContentFilter::create().with_display(&display).build()
        }

        Some(CaptureTarget::Process(pid)) => {
            let app = content.applications().into_iter()
                .find(|a| a.process_id() == pid)
                .ok_or_else(|| anyhow!("Application with PID {} not found", pid))?;
            let display = content.displays().into_iter().next().ok_or_else(|| anyhow!("No display found for app capture base"))?;
            SCContentFilter::create()
                .with_display(&display)
                .with_including_applications(&[&app], &[])
                .build()
        }

        None => {
            let display = content.displays().into_iter().next().ok_or_else(|| anyhow!("No display found"))?;
            SCContentFilter::create().with_display(&display).build()
        }
    };

    let mut sc_config = SCStreamConfiguration::new();
    sc_config.set_captures_audio(capture_system);
    sc_config.set_excludes_current_process_audio(true);
    sc_config.set_sample_rate(48000);
    sc_config.set_channel_count(2);

    if capture_mic {
        sc_config.set_captures_microphone(true);
    }

    let mut stream = SCStream::new(&filter, &sc_config);

    if capture_system {
        stream.add_output_handler(AudioOutputHandler { 
            tx: tx.clone(), 
            source_type: AudioSourceType::System 
        }, SCStreamOutputType::Audio);
    }

    if capture_mic {
        stream.add_output_handler(AudioOutputHandler { 
            tx, 
            source_type: AudioSourceType::Microphone 
        }, SCStreamOutputType::Microphone);
    }

    stream.start_capture().map_err(|e| anyhow!("Failed to start capture: {}", e))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::cm_time_to_ns;

    #[test]
    fn cm_time_to_ns_converts_basic_values() {
        assert_eq!(cm_time_to_ns(1, 1), 1_000_000_000);
        assert_eq!(cm_time_to_ns(480, 48_000), 10_000_000);
    }

    #[test]
    fn cm_time_to_ns_rejects_invalid_input() {
        assert_eq!(cm_time_to_ns(-1, 1), 0);
        assert_eq!(cm_time_to_ns(1, 0), 0);
        assert_eq!(cm_time_to_ns(1, -1), 0);
    }

    #[test]
    fn cm_time_to_ns_handles_large_values_without_overflow() {
        let large = i64::MAX;
        let out = cm_time_to_ns(large, 48_000);
        assert!(out > 0);
    }
}
