use std::collections::HashMap;
use tokio::sync::mpsc;

/// Bounded, backpressure-aware message bus for inter-actor messaging.
///
/// Each channel is bounded (default 1024). `send` fails fast when full,
/// preventing unbounded memory growth under flood. Use `try_send` for
/// non-blocking or `send_timeout` for bounded wait.
pub struct MessageBus {
    channels: HashMap<String, mpsc::Sender<Vec<u8>>>,
    // Receivers are held here so the channel stays alive after register();
    // dropping the receiver would make sends fail silently. We use bounded channels
    // but this vector is only for lifetime — callers should use `create_channel` for real receivers.
    _receivers: Vec<mpsc::Receiver<Vec<u8>>>,
    bound: usize,
}

impl MessageBus {
    pub fn new() -> Self {
        Self::with_bound(1024)
    }

    pub fn with_bound(bound: usize) -> Self {
        Self {
            channels: HashMap::new(),
            _receivers: Vec::new(),
            bound: bound.max(1),
        }
    }

    /// Create a real consumer channel — caller owns the Receiver.
    pub fn create_channel(&mut self, name: &str) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(self.bound);
        self.channels.insert(name.to_string(), tx);
        rx
    }

    /// Non-blocking send; returns false if channel missing or full.
    pub fn try_send(&self, name: &str, data: Vec<u8>) -> bool {
        if let Some(tx) = self.channels.get(name) {
            tx.try_send(data).is_ok()
        } else {
            false
        }
    }

    /// Legacy unbounded-style send (delegates to try_send for compat).
    pub fn send(&self, name: &str, data: Vec<u8>) -> bool {
        self.try_send(name, data)
    }

    /// Async send with backpressure.
    pub async fn send_async(&self, name: &str, data: Vec<u8>) -> bool {
        if let Some(tx) = self.channels.get(name) {
            tx.send(data).await.is_ok()
        } else {
            false
        }
    }

    pub fn register(&mut self, name: &str) -> mpsc::Sender<Vec<u8>> {
        let (tx, rx) = mpsc::channel(self.bound);
        self.channels.insert(name.to_string(), tx.clone());
        self._receivers.push(rx);
        tx
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }
    pub fn bound(&self) -> usize {
        self.bound
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Actor {
    pub name: String,
    pub mailbox: mpsc::Receiver<Vec<u8>>,
}

impl Actor {
    pub fn new(name: String, mailbox: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { name, mailbox }
    }

    pub async fn receive(&mut self) -> Option<Vec<u8>> {
        self.mailbox.recv().await
    }
}
