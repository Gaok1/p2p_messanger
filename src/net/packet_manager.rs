use std::{
    collections::HashMap,
    thread,
    time::{Duration, Instant},
};

use bincode::Options;
use laminar::{ErrorKind, Packet, Socket};

use super::bincode_options;

const HEADER_SIZE_ESTIMATE: usize = 16; // aproximacao conservadora para ids e contadores

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PacketFragment {
    pub message_id: u64,
    pub fragment_index: u16,
    pub total_fragments: u16,
    pub data: Vec<u8>,
}

#[derive(Debug)]
struct FragmentBuffer {
    _total: u16,
    received: Vec<Option<Vec<u8>>>,
    last_update: Instant,
}

impl FragmentBuffer {
    fn new(total: u16) -> Self {
        Self {
            _total: total,
            received: vec![None; total as usize],
            last_update: Instant::now(),
        }
    }

    fn insert(&mut self, idx: u16, data: Vec<u8>) {
        if let Some(slot) = self.received.get_mut(idx as usize) {
            *slot = Some(data);
        }
        self.last_update = Instant::now();
    }

    fn is_complete(&self) -> bool {
        self.received.iter().all(|entry| entry.is_some())
    }

    fn assemble(self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(
            self.received
                .iter()
                .filter_map(|entry| entry.as_ref())
                .map(|part| part.len())
                .sum(),
        );

        for part in self.received.into_iter().flatten() {
            payload.extend_from_slice(&part);
        }

        payload
    }
}

#[derive(Debug)]
pub(crate) struct RateLimiter {
    capacity: u64,
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.capacity as f64).min(self.capacity as f64);
        self.last_refill = now;
    }

    pub(crate) fn wait_for(&mut self, cost: u64) {
        loop {
            self.refill();
            if self.tokens >= cost as f64 {
                self.tokens -= cost as f64;
                break;
            }
            let missing = cost as f64 - self.tokens;
            let sleep_ms = ((missing / self.capacity as f64) * 1000.0).ceil() as u64;
            thread::sleep(Duration::from_millis(sleep_ms.max(1)));
        }
    }
}

/// Responsável por fragmentar e reordenar mensagens para evitar dependência
/// do mecanismo interno do Laminar.
#[derive(Debug)]
pub(crate) struct PacketManager {
    next_message_id: u64,
    max_payload: usize,
    rate_limiter: RateLimiter,
    fragments: HashMap<u64, FragmentBuffer>,
    fragment_ttl: Duration,
}

impl PacketManager {
    pub(crate) fn new(max_payload: usize, bytes_per_second: u64) -> Self {
        Self {
            next_message_id: 1,
            max_payload,
            rate_limiter: RateLimiter::new(bytes_per_second),
            fragments: HashMap::new(),
            fragment_ttl: Duration::from_secs(30),
        }
    }

    pub(crate) fn send(
        &mut self,
        socket: &mut Socket,
        peer: std::net::SocketAddr,
        payload: Vec<u8>,
        channel_id: u8,
    ) -> laminar::Result<()> {
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1).max(1);

        let fragments = self.fragment_payload(message_id, payload);
        for fragment in fragments {
            let encoded = bincode_options().serialize(&fragment).map_err(|err| {
                laminar::ErrorKind::IOError(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("erro ao serializar fragmento: {err}"),
                ))
            })?;

            let cost = (encoded.len() + HEADER_SIZE_ESTIMATE) as u64;
            self.rate_limiter.wait_for(cost);

            let packet = Packet::reliable_ordered(peer, encoded, Some(channel_id));
            self.send_packet(socket, packet)?;
        }

        Ok(())
    }

    fn fragment_payload(&self, message_id: u64, payload: Vec<u8>) -> Vec<PacketFragment> {
        if payload.len() <= self.max_payload {
            return vec![PacketFragment {
                message_id,
                fragment_index: 0,
                total_fragments: 1,
                data: payload,
            }];
        }

        let mut fragments = Vec::new();
        let total_fragments = ((payload.len() + self.max_payload - 1) / self.max_payload) as u16;

        for (idx, chunk) in payload.chunks(self.max_payload).enumerate() {
            let fragment = PacketFragment {
                message_id,
                fragment_index: idx as u16,
                total_fragments,
                data: chunk.to_vec(),
            };
            fragments.push(fragment);
        }

        fragments
    }

    fn send_packet(&self, socket: &mut Socket, packet: Packet) -> laminar::Result<()> {
        const MAX_RETRIES: usize = 50;
        const BACKOFF: Duration = Duration::from_millis(5);
        let mut retries = 0;

        loop {
            match socket.send(packet.clone()) {
                Ok(()) => return Ok(()),
                Err(ErrorKind::IOError(err))
                    if err.kind() == std::io::ErrorKind::WouldBlock && retries < MAX_RETRIES =>
                {
                    retries += 1;
                    socket.manual_poll(Instant::now());
                    thread::sleep(BACKOFF);
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub(crate) fn process_incoming(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let fragment: PacketFragment = bincode_options()
            .deserialize(payload)
            .map_err(|err| err.to_string())?;

        if fragment.total_fragments == 1 {
            return Ok(Some(fragment.data));
        }

        let complete;
        {
            let buffer = self
                .fragments
                .entry(fragment.message_id)
                .or_insert_with(|| FragmentBuffer::new(fragment.total_fragments));
            buffer.insert(fragment.fragment_index, fragment.data);
            complete = buffer.is_complete();
        }

        if complete {
            if let Some(buffer) = self.fragments.remove(&fragment.message_id) {
                let assembled = buffer.assemble();
                self.cleanup_old_entries();
                return Ok(Some(assembled));
            }
        }

        self.cleanup_old_entries();
        Ok(None)
    }

    fn cleanup_old_entries(&mut self) {
        let now = Instant::now();
        self.fragments
            .retain(|_, buffer| now.duration_since(buffer.last_update) < self.fragment_ttl);
    }
}
