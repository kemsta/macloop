use core_engine::{
    AudioEngineController, MicInfo, MicrophoneSource, MicrophoneSourceConfig, SourceType,
    WavFileOutput, MASTER_FORMAT,
};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
struct CliArgs {
    out_path: String,
    seconds: f32,
    device_id: Option<u32>,
    list_mics: bool,
    no_vpio: bool,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            out_path: "out/mic.wav".to_string(),
            seconds: 5.0,
            device_id: None,
            list_mics: false,
            no_vpio: false,
        }
    }
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = CliArgs::default();
    let mut it = std::env::args().skip(1);

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => {
                let Some(value) = it.next() else {
                    return Err("--out requires a path".to_string());
                };
                args.out_path = value;
            }
            "--seconds" => {
                let Some(value) = it.next() else {
                    return Err("--seconds requires a number".to_string());
                };
                args.seconds = value
                    .parse::<f32>()
                    .map_err(|_| format!("invalid seconds value: {value}"))?;
            }
            "--device-id" => {
                let Some(value) = it.next() else {
                    return Err("--device-id requires a u32 value".to_string());
                };
                args.device_id = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid device-id: {value}"))?,
                );
            }
            "--list-mics" => {
                args.list_mics = true;
            }
            "--no-vpio" => {
                args.no_vpio = true;
            }
            "--help" | "-h" => {
                return Err(
                    "Usage: cli_tester [--out PATH] [--seconds N] [--device-id ID] [--list-mics] [--no-vpio]"
                        .to_string(),
                );
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    if args.seconds <= 0.0 {
        return Err("seconds must be > 0".to_string());
    }

    Ok(args)
}

fn print_mic_info(mic: &MicInfo) {
    println!(
        "  id={} default={} name={}",
        mic.id, mic.is_default, mic.name
    );
}

fn print_mics() {
    let mics = MicrophoneSource::list_mics();
    if mics.is_empty() {
        println!("No microphones found.");
    } else {
        println!("Microphones:");
        for mic in &mics {
            print_mic_info(mic);
        }
    }
}

fn ensure_parent_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn build_single_config(args: &CliArgs) -> MicrophoneSourceConfig {
    MicrophoneSourceConfig {
        device_id: args.device_id,
        vpio_enabled: !args.no_vpio,
    }
}

fn record_one_file(
    out_path: &Path,
    seconds: f32,
    mic_cfg: MicrophoneSourceConfig,
) -> Result<(), Box<dyn Error>> {
    let mut engine = AudioEngineController::new(
        64,                                                                        // command queue
        128,                                                                       // garbage queue
        MASTER_FORMAT.sample_rate as usize * MASTER_FORMAT.channels as usize * 30, // route ring (~30s)
    );

    let stream_id = "mic".to_string();
    let output_id = "wav".to_string();

    let pipeline = engine.create_stream(
        stream_id.clone(),
        SourceType::Microphone {
            device_id: mic_cfg.device_id,
        },
        32,
        8,
    )?;
    engine.route(&stream_id, &output_id)?;

    let consumer = engine
        .take_output_consumer(&output_id)
        .ok_or("failed to get consumer for WAV output")?;

    let mut wav_out = WavFileOutput::spawn_path(out_path, MASTER_FORMAT, consumer)?;
    let mut mic = MicrophoneSource::new(pipeline, mic_cfg)?;

    println!(
        "Capture mode: vpio_enabled={} device_id={:?}",
        mic_cfg.vpio_enabled, mic_cfg.device_id
    );
    println!(
        "Recording microphone to {} ({} sec)...",
        out_path.display(),
        seconds
    );
    mic.start()?;
    thread::sleep(Duration::from_secs_f32(seconds));
    mic.stop()?;
    // Ensure AudioUnit is fully dropped before stopping WAV writer thread.
    drop(mic);
    wav_out.stop()?;
    engine.tick_gc();

    let stats = engine.get_stats();
    if let Some(s) = stats.get(&stream_id) {
        println!(
            "Stats: callback_us={} dropped_frames={} buffer_size={}",
            s.pipeline.total_callback_time_us, s.pipeline.dropped_frames, s.pipeline.buffer_size
        );
    }

    println!("Done: {}", out_path.display());
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = parse_args().map_err(|e| format!("{e}"))?;

    if args.list_mics {
        print_mics();
        return Ok(());
    }

    let out_path = PathBuf::from(&args.out_path);
    ensure_parent_dir(&out_path)?;
    let cfg = build_single_config(&args);
    record_one_file(&out_path, args.seconds, cfg)
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
