from __future__ import annotations

import re
from pathlib import Path

from macloop._macloop import AudioProcessingConfig


def _pyi_defaults() -> dict[str, object]:
    pyi_path = Path(__file__).resolve().parents[1] / "macloop" / "_macloop.pyi"
    text = pyi_path.read_text(encoding="utf-8")

    patterns = {
        "sample_rate": r"sample_rate:\s*int\s*=\s*(\d+)",
        "channels": r"channels:\s*int\s*=\s*(\d+)",
        "enable_aec": r"enable_aec:\s*bool\s*=\s*(True|False)",
        "enable_ns": r"enable_ns:\s*bool\s*=\s*(True|False)",
        "sample_format": r"sample_format:\s*str\s*=\s*\"([^\"]+)\"",
        "aec_stream_delay_ms": r"aec_stream_delay_ms:\s*int\s*=\s*(-?\d+)",
        "aec_auto_delay_tuning": r"aec_auto_delay_tuning:\s*bool\s*=\s*(True|False)",
        "aec_max_delay_ms": r"aec_max_delay_ms:\s*int\s*=\s*(\d+)",
    }

    result: dict[str, object] = {}
    for key, pattern in patterns.items():
        match = re.search(pattern, text)
        assert match is not None, f"Could not find default for {key} in _macloop.pyi"
        raw = match.group(1)
        if raw in {"True", "False"}:
            result[key] = raw == "True"
        elif key == "sample_format":
            result[key] = raw
        else:
            result[key] = int(raw)
    return result


def test_audio_processing_config_defaults_match_pyi_and_runtime() -> None:
    pyi = _pyi_defaults()
    runtime = AudioProcessingConfig()

    assert runtime.sample_rate == pyi["sample_rate"]
    assert runtime.channels == pyi["channels"]
    assert runtime.enable_aec == pyi["enable_aec"]
    assert runtime.enable_ns == pyi["enable_ns"]
    assert runtime.sample_format == pyi["sample_format"]
    assert runtime.aec_stream_delay_ms == pyi["aec_stream_delay_ms"]
    assert runtime.aec_auto_delay_tuning == pyi["aec_auto_delay_tuning"]
    assert runtime.aec_max_delay_ms == pyi["aec_max_delay_ms"]
