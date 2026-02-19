use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct ConnectAttempt {
    pub id: u64,
    pub peer: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct ConnectResult {
    pub id: u64,
    pub peer: SocketAddr,
    pub connection: Option<quinn::Connection>,
}

#[derive(Clone, Copy, Debug)]
pub struct MobilityConfig {
    pub enabled: bool,
    pub observe_interval: Duration,
    pub reconnect_enabled: bool,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    pub rebind_after_failures: u32,
}

const MOBILITY_DEFAULT: MobilityConfig = MobilityConfig {
    enabled: true,
    observe_interval: Duration::from_secs(10),
    reconnect_enabled: true,
    reconnect_initial: Duration::from_millis(500),
    reconnect_max: Duration::from_secs(10),
    rebind_after_failures: 0,
};

impl Default for MobilityConfig {
    fn default() -> Self {
        MOBILITY_DEFAULT
    }
}

#[derive(Debug)]
pub struct ReconnectState {
    pub peer: SocketAddr,
    pub backoff: Duration,
    pub failures: u32,
}

#[derive(Debug)]
pub enum ConnSignal {
    Closed { peer: SocketAddr, error: String },
}

impl MobilityConfig {
    pub fn should_rebind(&self, failures: u32) -> bool {
        failures >= self.rebind_after_failures
    }

    pub fn next_backoff(&self, current: Duration) -> Duration {
        (current * 2).min(self.reconnect_max)
    }
}

impl ReconnectState {
    pub fn bump_backoff(&mut self, mobility: &MobilityConfig) {
        self.failures = self.failures.saturating_add(1);
        self.backoff = mobility.next_backoff(self.backoff);
    }
}
