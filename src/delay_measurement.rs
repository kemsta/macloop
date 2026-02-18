use std::collections::HashMap;

/// Delay measurement with histogram tracking
#[derive(Debug)]
pub struct DelayMeasurement {
    histogram: DelayHistogram,
}

/// Histogram for delay measurements
#[derive(Debug)]
pub struct DelayHistogram {
    // Buckets in microseconds: <1ms, <2ms, <5ms, <10ms, <20ms, <50ms, <100ms, >100ms
    buckets: [u64; 8],
    bucket_limits_us: [u64; 7], // Upper limits for first 7 buckets
    total_samples: u64,
    sum_delay_us: u64,
    min_delay_us: u64,
    max_delay_us: u64,
}

impl DelayHistogram {
    pub fn new() -> Self {
        Self {
            buckets: [0; 8],
            bucket_limits_us: [1000, 2000, 5000, 10000, 20000, 50000, 100000], // μs
            total_samples: 0,
            sum_delay_us: 0,
            min_delay_us: u64::MAX,
            max_delay_us: 0,
        }
    }
    
    pub fn record(&mut self, delay_ns: u64) {
        let delay_us = delay_ns / 1000;
        
        // Update min/max
        self.min_delay_us = self.min_delay_us.min(delay_us);
        self.max_delay_us = self.max_delay_us.max(delay_us);
        
        // Update sum and count
        self.sum_delay_us += delay_us;
        self.total_samples += 1;
        
        // Find bucket
        let mut bucket_idx = self.bucket_limits_us.len(); // Default to last bucket (>100ms)
        for (i, &limit) in self.bucket_limits_us.iter().enumerate() {
            if delay_us < limit {
                bucket_idx = i;
                break;
            }
        }
        self.buckets[bucket_idx] += 1;
    }
    
}

impl DelayMeasurement {
    pub fn new() -> Self {
        Self {
            histogram: DelayHistogram::new(),
        }
    }
    
    pub fn record_delay(&mut self, delay_ns: u64) {
        self.histogram.record(delay_ns);
    }
}

/// Global delay measurements for different pipeline stages
pub struct DelayTracker {
    measurements: HashMap<String, DelayMeasurement>,
}

impl DelayTracker {
    pub fn new() -> Self {
        let mut measurements = HashMap::new();
        measurements.insert("total_pipeline".to_string(), DelayMeasurement::new());
        measurements.insert("processing_time".to_string(), DelayMeasurement::new());
        measurements.insert("timestamp_processor".to_string(), DelayMeasurement::new());
        measurements.insert("webrtc_resample_processor".to_string(), DelayMeasurement::new());
        measurements.insert("quantizer_processor".to_string(), DelayMeasurement::new());
        measurements.insert("aec_processor".to_string(), DelayMeasurement::new());
        measurements.insert("ns_processor".to_string(), DelayMeasurement::new());
        measurements.insert("resample_processor".to_string(), DelayMeasurement::new());
        
        Self { measurements }
    }
    
    pub fn record(&mut self, stage: &str, delay_ns: u64) {
        if let Some(measurement) = self.measurements.get_mut(stage) {
            measurement.record_delay(delay_ns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_records_samples() {
        let mut h = DelayHistogram::new();
        h.record(500_000);   // 0.5ms
        h.record(1_500_000); // 1.5ms
        h.record(12_000_000); // 12ms

        assert_eq!(h.total_samples, 3);
        assert_eq!(h.buckets[0], 1);
        assert_eq!(h.buckets[1], 1);
        assert_eq!(h.buckets[4], 1);
    }

    #[test]
    fn tracker_ignores_unknown_stage() {
        let mut t = DelayTracker::new();
        t.record("unknown_stage", 1_000);
        // no panic means pass
        t.record("processing_time", 2_000);
    }
}
