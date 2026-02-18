use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct PathMetricsSnapshot {
    pub min_rtt: Duration,
    pub srtt: Duration,
    pub delivery_rate_max: f64,
    pub bdp_estimate: u64,
    pub target_inflight: u64,
}

#[derive(Debug, Default)]
struct PathMetrics {
    min_rtt: Option<Duration>,
    srtt: Duration,
    delivery_rate_max: f64,
    bdp_estimate: u64,
    last_bytes_sent: u64,
    last_sample: Option<Instant>,
    target_inflight: u64,
}

#[derive(Clone, Debug)]
pub struct AutotuneConfig {
    pub enabled: bool,
    pub gain: f64,
    pub min_window: u64,
    pub max_window: u64,
    pub sample_interval: Duration,
    pub rate_decay: f64,
}

impl Default for AutotuneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gain: 1.5,
            min_window: 1024 * 1024,
            max_window: 256 * 1024 * 1024,
            sample_interval: Duration::from_millis(500),
            rate_decay: 0.9,
        }
    }
}

impl AutotuneConfig {
    pub fn clamp_target(&self, value: u64) -> u64 {
        value.clamp(self.min_window, self.max_window)
    }
}

#[derive(Debug)]
pub struct AutotuneState {
    pub config: AutotuneConfig,
    pub metrics: PathMetrics,
}

impl AutotuneState {
    pub fn new(config: AutotuneConfig) -> Self {
        Self {
            metrics: PathMetrics {
                target_inflight: config.min_window,
                ..Default::default()
            },
            config,
        }
    }

    pub fn current_target(&self) -> u64 {
        self.metrics
            .target_inflight
            .max(self.config.min_window)
            .min(self.config.max_window)
    }

    pub fn update(&mut self, stats: &quinn::ConnectionStats, now: Instant) -> PathMetricsSnapshot {
        self.metrics.update(stats, now, &self.config);
        self.metrics.snapshot(&self.config)
    }
}

impl PathMetrics {
    fn update(&mut self, stats: &quinn::ConnectionStats, now: Instant, config: &AutotuneConfig) {
        let srtt = stats.path.rtt;
        self.srtt = srtt;
        self.min_rtt = Some(self.min_rtt.map(|min| min.min(srtt)).unwrap_or(srtt));

        if let Some(last_sample) = self.last_sample {
            let elapsed = now.saturating_duration_since(last_sample);
            if elapsed >= config.sample_interval / 2 {
                let delta = stats
                    .udp_tx
                    .bytes
                    .saturating_sub(self.last_bytes_sent)
                    .saturating_sub(0);
                let rate_sample = delta as f64 / elapsed.as_secs_f64().max(0.001);
                self.delivery_rate_max = if self.delivery_rate_max == 0.0 {
                    rate_sample
                } else {
                    let decayed = self.delivery_rate_max * config.rate_decay;
                    decayed.max(rate_sample)
                };
            }
        }

        self.last_sample = Some(now);
        self.last_bytes_sent = stats.udp_tx.bytes;

        let min_rtt = self.min_rtt.unwrap_or(srtt);
        let bdp = (self.delivery_rate_max * min_rtt.as_secs_f64()) as u64;
        self.bdp_estimate = bdp;
        let target = config.clamp_target(((bdp as f64) * config.gain) as u64);
        self.target_inflight = target;
    }

    fn snapshot(&self, config: &AutotuneConfig) -> PathMetricsSnapshot {
        PathMetricsSnapshot {
            min_rtt: self.min_rtt.unwrap_or_default(),
            srtt: self.srtt,
            delivery_rate_max: self.delivery_rate_max,
            bdp_estimate: self.bdp_estimate,
            target_inflight: self.target_inflight.max(config.min_window),
        }
    }
}

impl PathMetricsSnapshot {
    pub fn srtt(&self) -> Duration {
        self.srtt
    }

    pub fn target_inflight(&self) -> u64 {
        self.target_inflight
    }

    pub fn delivery_rate_max(&self) -> f64 {
        self.delivery_rate_max
    }
}
