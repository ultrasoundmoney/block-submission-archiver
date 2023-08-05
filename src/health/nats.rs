use super::HealthCheck;

#[derive(Debug, Clone)]
pub struct NatsHealth {
    nats: async_nats::Client,
}

impl NatsHealth {
    pub fn new(nats: async_nats::Client) -> Self {
        Self { nats }
    }
}

impl HealthCheck for NatsHealth {
    fn health_status(&self) -> (bool, String) {
        use async_nats::connection::State;
        match self.nats.connection_state() {
            State::Connected => (true, "healthy, connected".to_string()),
            State::Disconnected => (false, "unhealthy, disconnected".to_string()),
            State::Pending => (false, "unhealthy, reconnecting".to_string()),
        }
    }
}
