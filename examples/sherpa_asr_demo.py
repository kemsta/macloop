from __future__ import annotations

import argparse
from pathlib import Path

import numpy as np

import macloop


def download_sherpa_model(
    repo_id: str,
    cache_dir: str | None = None,
) -> Path:
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
        allow_patterns=[
            "*.onnx",
            "*.txt",
            "*.bpe",
            "*.vocab",
        ],
    )
    return Path(local_dir)


def find_file(model_dir: Path, patterns: list[str], required: bool = True) -> Path | None:
    for pattern in patterns:
        matches = sorted(model_dir.glob(pattern))
        if matches:
            return matches[0]
    if required:
        joined = ", ".join(patterns)
        raise FileNotFoundError(f"No file matched patterns: {joined} in {model_dir}")
    return None


def main() -> None:
    parser = argparse.ArgumentParser(description="Minimal macloop + sherpa-onnx ASR demo.")
    parser.add_argument("--seconds", type=float, default=5.0, help="How many seconds to capture from microphone.")
    parser.add_argument("--sample-rate", type=int, default=16_000, help="Capture sample rate.")
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
    cfg = macloop.AudioProcessingConfig(
        sample_rate=args.sample_rate,
        channels=1,
        sample_format="f32",
        enable_aec=False,
        enable_ns=False,
    )

    print(f"Streaming from microphone for ~{args.seconds:.1f}s...")
    print("Partial transcript:")

    import time

    started = time.monotonic()
    last_text = ""
    seen_audio = False

    with macloop.Capture(config=cfg, capture_system=False, capture_mic=True) as mic_stream:
        for chunk in mic_stream:
            if chunk.source != "mic":
                continue

            arr = np.asarray(chunk.samples, dtype=np.float32)
            if arr.size == 0:
                continue

            seen_audio = True
            stream.accept_waveform(args.sample_rate, arr)

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
