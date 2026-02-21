from __future__ import annotations

from typing import Any, Callable, Optional, TypeAlias

import numpy as np
import numpy.typing as npt

AudioSamples: TypeAlias = npt.NDArray[np.int16] | npt.NDArray[np.float32]


class AudioProcessingConfig:
    sample_rate: int
    channels: int
    enable_aec: bool
    enable_ns: bool
    sample_format: str
    aec_stream_delay_ms: int
    aec_auto_delay_tuning: bool
    aec_max_delay_ms: int

    def __init__(
        self,
        sample_rate: int = 48000,
        channels: int = 2,
        enable_aec: bool = False,
        enable_ns: bool = False,
        sample_format: str = "f32",
        aec_stream_delay_ms: int = 0,
        aec_auto_delay_tuning: bool = False,
        aec_max_delay_ms: int = 140,
    ) -> None: ...

    def calibrate_delay(self, measured_system_latency_ms: float, measured_mic_latency_ms: float) -> None: ...


class PipelineStats:
    frames_in_mic: int
    frames_in_system: int
    frames_out_mic: int
    frames_out_system: int
    processor_errors: int
    processor_drain_errors: int
    callback_errors: int
    gil_acquire_failures: int

    timestamp_avg_ms: float
    timestamp_max_ms: float
    webrtc_resample_avg_ms: float
    webrtc_resample_max_ms: float
    quantizer_avg_ms: float
    quantizer_max_ms: float
    aec_avg_ms: float
    aec_max_ms: float
    ns_avg_ms: float
    ns_max_ms: float
    processing_avg_ms: float
    processing_max_ms: float
    total_pipeline_avg_ms: float
    total_pipeline_max_ms: float

    aec_tune_enabled: bool
    aec_tune_frozen: bool
    aec_applied_delay_ms: int
    aec_best_delay_ms: int
    aec_step_ms: int
    aec_direction: int
    aec_interval_frames: int
    aec_max_delay_ms: int
    aec_last_erle: Optional[float]
    aec_erle_ema: Optional[float]
    aec_best_erle: Optional[float]
    aec_last_apm_delay_ms: Optional[int]
    aec_tune_events: int
    aec_rollback_events: int
    aec_freeze_events: int
    aec_skipped_inactive_mic: int
    aec_skipped_inactive_system: int


class AudioEngine:
    def __init__(
        self,
        display_id: Optional[int] = None,
        pid: Optional[int] = None,
        config: Optional[AudioProcessingConfig] = None,
    ) -> None: ...

    def start(
        self,
        callback: Callable[[str, AudioSamples], None],
        capture_system: bool = True,
        capture_mic: bool = False,
    ) -> None: ...

    def stop(self) -> None: ...

    def get_stats(self) -> PipelineStats: ...


def list_audio_sources() -> list[dict[str, Any]]: ...
