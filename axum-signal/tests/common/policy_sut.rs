use axum::extract::ws::Message;
use axum_signal::{BroadcastPolicy, InMemoryWsClients, JsonCodec, WsClients, WsHubConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct PolicySut {
    clients: Arc<InMemoryWsClients<(), JsonCodec>>,
}

pub struct WsConnection {
    id: &'static str,
    rx: mpsc::Receiver<Message>,
    pub cancel: CancellationToken,
    drops: Arc<AtomicU32>,
    /// Number of pre-filled messages to drain before counting real ones.
    prefilled: usize,
    clients: Arc<InMemoryWsClients<(), JsonCodec>>,
}

#[allow(dead_code)]
impl PolicySut {
    pub fn new(policy: BroadcastPolicy) -> Self {
        let config = WsHubConfig {
            policy,
            ..WsHubConfig::default()
        };
        let clients = Arc::new(InMemoryWsClients::<(), JsonCodec>::new(
            config.policy,
            config.broadcast_channel_capacity,
        ));
        Self { clients }
    }

    /// Normal connection with a roomy send buffer.
    pub async fn active(&self, id: &'static str) -> WsConnection {
        self.make_connection(id, 32, 0).await
    }

    /// Connection whose send buffer is pre-filled so all subsequent sends will block.
    pub async fn slow(&self, id: &'static str) -> WsConnection {
        self.make_connection(id, 1, 1).await
    }

    async fn make_connection(
        &self,
        id: &'static str,
        capacity: usize,
        prefill: usize,
    ) -> WsConnection {
        let drops = Arc::new(AtomicU32::new(0));
        let drops_for_cb = drops.clone();
        let (tx, rx) = mpsc::channel(capacity);
        for _ in 0..prefill {
            tx.try_send(Message::text("")).unwrap();
        }
        let cancel = CancellationToken::new();
        self.clients
            .add(
                Arc::from(id),
                tx,
                cancel.clone(),
                Arc::new(move |_| {
                    drops_for_cb.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .await;
        WsConnection {
            id,
            rx,
            cancel,
            drops,
            prefilled: prefill,
            clients: self.clients.clone(),
        }
    }

    pub async fn broadcast(&self) {
        self.clients.broadcast_except(&[], ()).await;
    }

    /// Injects RTT samples directly, bypassing the ping/pong exchange.
    ///
    /// Calls `update_rtt` once per element, so the rolling average stabilises after `rtt_samples`
    /// calls. For tests that need a single decisive measurement, pass a slice with one value.
    pub async fn set_rtt(&self, id: &str, samples: &[Duration]) {
        for &rtt in samples {
            self.clients.update_rtt(id, rtt).await;
        }
    }
}

#[allow(dead_code)]
impl WsConnection {
    /// Returns true if a real (non-prefill) message is waiting.
    pub fn has_message(&mut self) -> bool {
        self.drain_prefill();
        self.rx.try_recv().is_ok()
    }

    pub fn message_count(&mut self) -> usize {
        self.drain_prefill();
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    pub fn is_disconnected(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn drop_count(&self) -> u32 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Replaces the send buffer with a roomy channel, resetting the drop counter.
    /// The next broadcast will succeed, resetting the consecutive drop counter.
    pub async fn recover(&mut self) {
        let drops = Arc::new(AtomicU32::new(0));
        let drops_for_cb = drops.clone();
        let (tx, rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        self.clients
            .add(
                Arc::from(self.id),
                tx,
                cancel.clone(),
                Arc::new(move |_| {
                    drops_for_cb.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .await;
        self.rx = rx;
        self.cancel = cancel;
        self.drops = drops;
        self.prefilled = 0;
    }

    fn drain_prefill(&mut self) {
        while self.prefilled > 0 {
            self.rx.try_recv().ok();
            self.prefilled -= 1;
        }
    }
}
