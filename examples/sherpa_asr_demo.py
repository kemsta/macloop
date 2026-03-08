from __future__ import annotations

import argparse
import time
from pathlib import Path
from typing import Optional

import numpy as np
try:
    import macloop
except ModuleNotFoundError:
    from _bootstrap import bootstrap_repo_root

    bootstrap_repo_root()
    import macloop


def download_sherpa_model(repo_id: str, cache_dir: Optional[str] = None) -> Path:
    try:
        from huggingface_hub import snapshot_download
    except ImportError as exc:
        raise RuntimeError(
            "huggingface_hub is not installed. Run: "
            "uv run --with huggingface_hub --with sherpa-onnx --reinstall-package macloop "
            "python examples/sherpa_asr_demo.py"
        ) from exc

    local_dir = snapshot_download(
        repo_id=repo_id,
        cache_dir=cache_dir,
        allow_patterns=["*.onnx", "*.txt", "*.bpe", "*.vocab"],
    )
    return Path(local_dir)


def find_file(model_dir: Path, patterns: list[str], required: bool = True) -> Optional[Path]:
    for pattern in patterns:
        matches = sorted(model_dir.glob(pattern))
        if matches:
            return matches[0]
    if required:
        joined = ", ".join(patterns)
        raise FileNotFoundError(f"No file matched patterns: {joined} in {model_dir}")
    return None


def main() -> None:
    parser = argparse.ArgumentParser(description="Minimal macloop + sherpa-onnx microphone ASR demo.")
    parser.add_argument("--seconds", type=float, default=5.0, help="How long to capture from microphone.")
    parser.add_argument("--sample-rate", type=int, default=16_000, help="Output sample rate.")
    parser.add_argument("--chunk-frames", type=int, default=320, help="Frames per ASR chunk.")
    parser.add_argument("--device-id", type=int, default=None, help="Optional microphone device id.")
    parser.add_argument(
        "--repo-id",
        default="csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26",
        help="Hugging Face repo id with sherpa-onnx transducer model.",
    )
    parser.add_argument("--model-dir", default=None, help="Local sherpa model directory. If omitted, downloads from HF.")
    parser.add_argument("--hf-cache-dir", default=None, help="Optional Hugging Face cache directory.")
    args = parser.parse_args()

    try:
        import sherpa_onnx
    except ImportError as exc:
        raise RuntimeError(
            "sherpa-onnx is not installed. Run: "
            "uv run --with sherpa-onnx --with huggingface_hub --reinstall-package macloop "
            "python examples/sherpa_asr_demo.py"
        ) from exc

    if args.model_dir:
        model_dir = Path(args.model_dir)
    else:
        print(f"Downloading sherpa model from Hugging Face: {args.repo_id}")
        model_dir = download_sherpa_model(args.repo_id, cache_dir=args.hf_cache_dir)

    encoder = find_file(model_dir, ["*encoder*.onnx"])
    decoder = find_file(model_dir, ["*decoder*.onnx"])
    joiner = find_file(model_dir, ["*joiner*.onnx"])
    tokens = find_file(model_dir, ["tokens.txt", "*.tokens.txt", "*.txt"])

    print(f"Loading sherpa-onnx model from: {model_dir}")
    recognizer = sherpa_onnx.OnlineRecognizer.from_transducer(
        encoder=str(encoder),
        decoder=str(decoder),
        joiner=str(joiner),
        tokens=str(tokens),
        num_threads=1,
        sample_rate=args.sample_rate,
        provider="cpu",
        decoding_method="greedy_search",
    )

    stream = recognizer.create_stream()
    print(f"Streaming from microphone for ~{args.seconds:.1f}s...")
    print("Partial transcript:")

    started = time.monotonic()
    last_text = ""
    seen_audio = False
    vpio_enabled = args.device_id is None

    with macloop.AudioEngine() as engine:
        mic = engine.create_stream(
            macloop.MicrophoneSource,
            device_id=args.device_id,
            vpio_enabled=vpio_enabled,
        )
        mic_for_asr = engine.route(stream=mic)

        with macloop.AsrSink(
            routes=[mic_for_asr],
            chunk_frames=args.chunk_frames,
            sample_rate=args.sample_rate,
            channels=1,
            sample_format="f32",
        ) as asr_sink:
            for chunk in asr_sink.chunks():
                samples = np.asarray(chunk.samples, dtype=np.float32)
                if samples.size == 0:
                    continue

                seen_audio = True
                stream.accept_waveform(args.sample_rate, samples)

                while recognizer.is_ready(stream):
                    recognizer.decode_stream(stream)

                text = recognizer.get_result(stream)
                if text and text != last_text:
                    print(text)
                    last_text = text

                if time.monotonic() - started >= args.seconds:
                    break

    if not seen_audio:
        raise RuntimeError("No microphone audio captured.")

    stream.input_finished()
    while recognizer.is_ready(stream):
        recognizer.decode_stream(stream)
    text = recognizer.get_result(stream).strip()

    print("\n=== TRANSCRIPT ===")
    print(text if text else "<empty>")


if __name__ == "__main__":
    main()
