mod common;

use anyhow::Result;
use common::server::TestCommand;
use common::sut::Sut;

#[tokio::test]
async fn unicast_delivers_message_only_to_sender() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;

    c1.send(TestCommand::Unicast {
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    c2.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_delivers_message_to_all_connections() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;

    c1.send(TestCommand::Broadcast {
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c2.recv().await?.payload, "hello");

    c1.disconnect().await;
    c2.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_except_skips_excluded_connection() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;
    let mut c3 = sut.connect().await?;

    let c2_id = c2.connection_id().await?;

    c1.send(TestCommand::BroadcastExcept {
        excluded: vec![c2_id],
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c3.recv().await?.payload, "hello");
    c2.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    c3.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn connection_receives_messages_after_joining_group() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;

    c1.add_to_group("room").await?;
    c2.add_to_group("room").await?;

    c1.send(TestCommand::BroadcastGroup {
        group: "room".into(),
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c2.recv().await?.payload, "hello");

    c1.disconnect().await;
    c2.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn connection_stops_receiving_after_leaving_group() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;

    c1.add_to_group("room").await?;
    c2.add_to_group("room").await?;
    c2.remove_from_group("room").await?;

    c1.send(TestCommand::BroadcastGroup {
        group: "room".into(),
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    c2.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_group_delivers_only_to_group_members() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;
    let mut c3 = sut.connect().await?;

    c1.add_to_group("room").await?;
    c2.add_to_group("room").await?;

    c1.send(TestCommand::BroadcastGroup {
        group: "room".into(),
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c2.recv().await?.payload, "hello");
    c3.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    c3.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_group_except_skips_excluded_member() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;
    let mut c3 = sut.connect().await?;

    c1.add_to_group("room").await?;
    c2.add_to_group("room").await?;
    c3.add_to_group("room").await?;

    let c2_id = c2.connection_id().await?;

    c1.send(TestCommand::BroadcastGroupExcept {
        group: "room".into(),
        excluded: vec![c2_id],
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c3.recv().await?.payload, "hello");
    c2.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    c3.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn broadcast_groups_delivers_to_union_without_duplicates() -> Result<()> {
    let sut = Sut::new().await?;
    let mut c1 = sut.connect().await?;
    let mut c2 = sut.connect().await?;
    let mut c3 = sut.connect().await?;
    let mut c4 = sut.connect().await?;

    c1.add_to_group("group1").await?;
    c2.add_to_group("group2").await?;
    c3.add_to_group("group1").await?;
    c3.add_to_group("group2").await?;
    // c4 is not in any group

    c1.send(TestCommand::BroadcastGroups {
        groups: vec!["group1".into(), "group2".into()],
        payload: "hello".into(),
    })?;

    assert_eq!(c1.recv().await?.payload, "hello");
    assert_eq!(c2.recv().await?.payload, "hello");
    assert_eq!(c3.recv().await?.payload, "hello");
    c3.assert_no_message().await?; // c3 is in both groups — must receive exactly once
    c4.assert_no_message().await?;

    c1.disconnect().await;
    c2.disconnect().await;
    c3.disconnect().await;
    c4.disconnect().await;
    Ok(())
}
