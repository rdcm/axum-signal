mod common;

use anyhow::Result;
use axum_signal::BroadcastPolicy::{Block, DropConnection, DropMessage, DropOnHighRtt};
use common::policy_sut::PolicySut;
use common::server::TestCommand;
use common::sut::Sut;
use std::time::Duration;

#[tokio::test]
async fn block_delivers_all_messages() {
    let sut = PolicySut::new(Block);
    let mut c = sut.active("c1").await;

    for _ in 0..5 {
        sut.broadcast().await;
    }

    assert_eq!(c.message_count(), 5);
    assert_eq!(c.drop_count(), 0);
}

#[tokio::test]
async fn drop_message_drops_and_keeps_connection() {
    let sut = PolicySut::new(DropMessage {
        timeout: Duration::from_millis(10),
    });
    let mut fast = sut.active("fast").await;
    let mut slow = sut.slow("slow").await;

    sut.broadcast().await;

    assert!(!fast.is_disconnected());
    assert!(fast.has_message());
    assert_eq!(fast.drop_count(), 0);

    assert!(!slow.is_disconnected());
    assert!(!slow.has_message());
    assert_eq!(slow.drop_count(), 1);
}

#[tokio::test]
async fn drop_connection_disconnects_after_max_drops() {
    let sut = PolicySut::new(DropConnection {
        timeout: Duration::from_millis(10),
        max_drops: 3,
    });
    let fast = sut.active("fast").await;
    let slow = sut.slow("slow").await;

    for _ in 0..3 {
        sut.broadcast().await;
    }

    assert!(!fast.is_disconnected());
    assert_eq!(fast.drop_count(), 0);

    assert!(slow.is_disconnected());
    assert_eq!(slow.drop_count(), 3);
}

#[tokio::test]
async fn drop_connection_resets_counter_on_successful_send() {
    let sut = PolicySut::new(DropConnection {
        timeout: Duration::from_millis(10),
        max_drops: 3,
    });
    let fast = sut.active("fast").await;
    let mut slow = sut.slow("slow").await;

    for _ in 0..2 {
        sut.broadcast().await;
    }
    assert!(!fast.is_disconnected());
    assert!(!slow.is_disconnected());

    // Successful send resets the consecutive drop counter to zero.
    slow.recover().await;
    sut.broadcast().await;

    assert!(!slow.is_disconnected());
    assert_eq!(slow.drop_count(), 0);

    // One more drop after reset — must not disconnect (1 < max_drops=3).
    slow = sut.slow("slow").await;
    sut.broadcast().await;
    assert!(!slow.is_disconnected());
    assert_eq!(slow.drop_count(), 1);
}

#[tokio::test]
async fn drop_on_high_rtt_delivers_when_rtt_unknown() {
    // No RTT measured yet — connection should receive messages normally.
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let mut c = sut.active("c1").await;

    sut.broadcast().await;

    assert!(!c.is_disconnected());
    assert!(c.has_message());
    assert_eq!(c.drop_count(), 0);
}

#[tokio::test]
async fn drop_on_high_rtt_delivers_when_rtt_within_limit() {
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let mut c = sut.active("c1").await;

    sut.set_rtt("c1", &[Duration::from_millis(20)]).await;
    sut.broadcast().await;

    assert!(!c.is_disconnected());
    assert!(c.has_message());
    assert_eq!(c.drop_count(), 0);
}

#[tokio::test]
async fn drop_on_high_rtt_disconnects_when_rtt_exceeded() {
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let fast = sut.active("fast").await;
    let mut slow = sut.active("slow").await;

    sut.set_rtt("slow", &[Duration::from_millis(100)]).await;
    sut.broadcast().await;

    // Slow connection should be disconnected.
    assert!(slow.is_disconnected());
    assert_eq!(slow.drop_count(), 1);
    assert!(!slow.has_message());

    // Fast connection (no RTT set) should be unaffected.
    assert!(!fast.is_disconnected());
}

#[tokio::test]
async fn drop_on_high_rtt_only_drops_high_rtt_connections() {
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let mut ok = sut.active("ok").await;
    let high = sut.active("high").await;

    sut.set_rtt("ok", &[Duration::from_millis(10)]).await;
    sut.set_rtt("high", &[Duration::from_millis(200)]).await;
    sut.broadcast().await;

    assert!(!ok.is_disconnected());
    assert!(ok.has_message());

    assert!(high.is_disconnected());
    assert_eq!(high.drop_count(), 1);
}

#[tokio::test]
async fn drop_on_high_rtt_spike_does_not_drop_when_average_is_within_limit() {
    // window=4, max=50ms. Three good samples (10ms) + one spike (150ms) → avg = 45ms < 50ms.
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let mut c = sut.active("c1").await;

    sut.set_rtt(
        "c1",
        &[
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(150),
        ],
    )
    .await;
    sut.broadcast().await;

    assert!(!c.is_disconnected());
    assert!(c.has_message());
    assert_eq!(c.drop_count(), 0);
}

#[tokio::test]
async fn drop_on_high_rtt_sustained_high_rtt_drops_connection() {
    // window=4, max=50ms. All four samples above limit → avg = 100ms > 50ms.
    let sut = PolicySut::new(DropOnHighRtt {
        max_rtt: Duration::from_millis(50),
        window: 4,
    });
    let c = sut.active("c1").await;

    sut.set_rtt(
        "c1",
        &[
            Duration::from_millis(100),
            Duration::from_millis(100),
            Duration::from_millis(100),
            Duration::from_millis(100),
        ],
    )
    .await;
    sut.broadcast().await;

    assert!(c.is_disconnected());
    assert_eq!(c.drop_count(), 1);
}

#[tokio::test]
async fn integration_block_delivers_to_all() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;

    for i in 0..5u32 {
        c1.send(TestCommand::BroadcastExcept {
            excluded: vec![],
            payload: i.to_string(),
        })?;
    }
    for i in 0..5u32 {
        assert_eq!(c1.recv().await?.payload, i.to_string());
        assert_eq!(c2.recv().await?.payload, i.to_string());
    }

    c1.disconnect().await;
    c2.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn integration_drop_message_active_clients_unaffected() -> Result<()> {
    let sut = Sut::new_with_policy(DropMessage {
        timeout: Duration::from_millis(50),
    })
    .await?;
    let mut c1 = sut.connect().await?;
    let mut c3 = sut.connect().await?;

    for i in 0..5u32 {
        c1.send(TestCommand::BroadcastExcept {
            excluded: vec![],
            payload: i.to_string(),
        })?;
    }
    for i in 0..5u32 {
        assert_eq!(c1.recv().await?.payload, i.to_string());
        assert_eq!(c3.recv().await?.payload, i.to_string());
    }

    c1.disconnect().await;
    c3.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn integration_drop_connection_active_clients_unaffected() -> Result<()> {
    let sut = Sut::new_with_policy(DropConnection {
        timeout: Duration::from_millis(50),
        max_drops: 2,
    })
    .await?;
    let mut c1 = sut.connect().await?;
    let mut c3 = sut.connect().await?;

    for i in 0..5u32 {
        c1.send(TestCommand::BroadcastExcept {
            excluded: vec![],
            payload: i.to_string(),
        })?;
    }
    for i in 0..5u32 {
        assert_eq!(c1.recv().await?.payload, i.to_string());
        assert_eq!(c3.recv().await?.payload, i.to_string());
    }

    c1.disconnect().await;
    c3.disconnect().await;
    Ok(())
}
