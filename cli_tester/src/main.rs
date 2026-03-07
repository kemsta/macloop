use core_engine::config::AudioProcessingConfig;
use core_engine::messages::{AudioFrame, AudioSourceType};
use core_engine::modular_pipeline::ModularPipeline;
use core_engine::stats::RuntimeStatsHandle;

fn build_frame(source: AudioSourceType, timestamp_ns: u64, samples_per_channel: usize, channels: u16) -> AudioFrame {
    let total = samples_per_channel * channels as usize;
    let samples = (0..total)
        .map(|i| ((i as f32 / 10.0).sin() * 0.2).clamp(-1.0, 1.0))
        .collect::<Vec<_>>();

    AudioFrame {
        source,
        samples,
        sample_rate: 48_000,
        channels,
        timestamp: timestamp_ns,
    }
}

fn main() {
    let mut cfg = AudioProcessingConfig::default();
    cfg.sample_rate = 16_000;
    cfg.channels = 1;

    let (tx, rx) = crossbeam_channel::unbounded();
    let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let stats = RuntimeStatsHandle::new();
    let stats_for_thread = stats.clone();

    let worker = std::thread::spawn(move || {
        let mut out_mic = 0_u64;
        let mut out_sys = 0_u64;
        let mut pipeline = ModularPipeline::new(rx, stop_rx, cfg, stats_for_thread);
        pipeline.run_with_handler(|frame| match frame.source {
            AudioSourceType::Microphone => out_mic += 1,
            AudioSourceType::System => out_sys += 1,
        });
        (out_mic, out_sys)
    });

    for i in 0..120_u64 {
        let ts = i * 10_000_000;
        let _ = tx.send(build_frame(AudioSourceType::System, ts, 480, 2));
        let _ = tx.send(build_frame(AudioSourceType::Microphone, ts, 480, 1));
    }

    drop(tx);
    let (out_mic, out_sys) = worker.join().expect("pipeline thread panicked");

    let snap = stats.snapshot();
    println!("core_engine smoke test complete");
    println!("frames in: mic={}, system={}", snap.frames_in_mic, snap.frames_in_system);
    println!("frames out: mic={}, system={}", out_mic, out_sys);
    println!("processor_errors={}, drain_errors={}", snap.processor_errors, snap.processor_drain_errors);
}
