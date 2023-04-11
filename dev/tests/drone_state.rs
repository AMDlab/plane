use chrono::{DateTime, NaiveDateTime, Utc};
use integration_test::integration_test;
use plane_controller::{
    drone_state::{apply_state_message, monitor_drone_state},
    state::{start_state_loop, StateHandle},
};
use plane_core::{
    messages::{
        agent::BackendState,
        cert::SetAcmeDnsRecord,
        state::{
            BackendMessage, BackendMessageType, ClusterStateMessage, DroneMessage,
            DroneMessageType, DroneMeta, WorldStateMessage,
        },
    },
    nats::TypedNats,
    types::{BackendId, ClusterName, DroneId},
    NeverResult,
};
use plane_dev::{
    resources::nats::Nats,
    timeout::{expect_to_stay_alive, LivenessGuard},
};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};
use tokio::time::sleep;

struct StateTestFixture {
    _nats: Nats,
    nats: TypedNats,
    pub state: StateHandle,
    _lg: LivenessGuard<NeverResult>,
}

impl StateTestFixture {
    async fn new() -> Self {
        let nats = Nats::new().await.unwrap();
        let conn = nats.connection().await.unwrap();
        let state = start_state_loop(conn.clone()).await.unwrap();
        let lg = expect_to_stay_alive(monitor_drone_state(conn.clone()));

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        Self {
            _nats: nats,
            state,
            nats: conn,
            _lg: lg,
        }
    }
}

#[integration_test]
async fn txt_record_from_drone() {
    let fixture = StateTestFixture::new().await;

    {
        let result = fixture
            .nats
            .request(&SetAcmeDnsRecord {
                cluster: ClusterName::new("plane.test".into()),
                value: "test123".into(),
            })
            .await
            .unwrap();

        assert!(result);

        let state = fixture.state.state();
        let cluster = state
            .cluster(&ClusterName::new("plane.test".into()))
            .unwrap();
        assert_eq!(cluster.txt_records.len(), 1);
        assert_eq!(cluster.txt_records.back().unwrap(), "test123");

        // NB: it's important that we (implicitly) drop state here,
        // because otherwise we will hit a deadlock when the state
        // synchronizer tries to acquire the lock.
    }

    {
        let result = fixture
            .nats
            .request(&SetAcmeDnsRecord {
                cluster: ClusterName::new("plane.test".into()),
                value: "test456".into(),
            })
            .await
            .unwrap();

        assert!(result);

        let state = fixture.state.state();
        let cluster = state
            .cluster(&ClusterName::new("plane.test".into()))
            .unwrap();
        assert_eq!(cluster.txt_records.len(), 2);
        assert_eq!(cluster.txt_records.back().unwrap(), "test456");
    }
}

#[integration_test]
async fn txt_records_different_clusters() {
    let fixture = StateTestFixture::new().await;

    let result = fixture
        .nats
        .request(&SetAcmeDnsRecord {
            cluster: ClusterName::new("plane.test".into()),
            value: "test123".into(),
        })
        .await
        .unwrap();
    assert!(result);
    let result = fixture
        .nats
        .request(&SetAcmeDnsRecord {
            cluster: ClusterName::new("abc.test".into()),
            value: "test456".into(),
        })
        .await
        .unwrap();
    assert!(result);
    let state = fixture.state.state();

    {
        let cluster = state
            .cluster(&ClusterName::new("plane.test".into()))
            .unwrap();
        assert_eq!(cluster.txt_records.len(), 1);
        assert_eq!(cluster.txt_records.back().unwrap(), "test123");
    }

    {
        let cluster = state.cluster(&ClusterName::new("abc.test".into())).unwrap();
        assert_eq!(cluster.txt_records.len(), 1);
        assert_eq!(cluster.txt_records.back().unwrap(), "test456");
    }
}

fn timestamp(t: u64) -> DateTime<Utc> {
    // Return timestamp t seconds after epoch.
    DateTime::from_utc(NaiveDateTime::from_timestamp_opt(t as i64, 0).unwrap(), Utc)
}

#[integration_test]
async fn status_lifecycle() {
    let fixture = StateTestFixture::new().await;
    let ip: IpAddr = Ipv4Addr::new(12, 12, 12, 12).into();
    let cluster = ClusterName::new("plane.test".into());
    let drone = DroneId::new_random();
    let backend = BackendId::new_random();

    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::DroneMessage(DroneMessage {
                drone: drone.clone(),
                message: DroneMessageType::Metadata(DroneMeta {
                    git_hash: None,
                    version: "0.1.0".into(),
                    ip,
                }),
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the drone exists.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let drone = cluster.drone(&drone).unwrap();
        assert_eq!(drone.meta.as_ref().unwrap().ip, ip);
    }

    // Assign a backend to the drone.
    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::Assignment {
                    drone: drone.clone(),
                },
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the backend has been assigned to the drone.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(backend.drone.as_ref().unwrap(), &drone);
    }

    // Update the state of the backend to "starting".
    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Starting,
                    timestamp: timestamp(1),
                },
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the the backend is in "starting" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(backend.state().unwrap(), BackendState::Starting);
    }

    // Update the state of the backend to "loading".
    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Loading,
                    timestamp: timestamp(2),
                },
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the the backend is in "loading" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(backend.state().unwrap(), BackendState::Loading);
    }

    // Update the state of the backend to "ready".
    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Ready,
                    timestamp: timestamp(3),
                },
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the the backend is in "ready" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(backend.state().unwrap(), BackendState::Ready);
    }

    // Update the state of the backend to "swept".
    apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Swept,
                    timestamp: timestamp(4),
                },
            }),
        },
    )
    .await
    .unwrap();

    {
        // Ensure that the the backend is in "swept" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(backend.state().unwrap(), BackendState::Swept);
    }

    // Ensure that if we reconstruct the state from NATS, we have the same state history.
    let state1 = start_state_loop(fixture.nats.clone()).await.unwrap();
    {
        let state = state1.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        let state_vec: Vec<(DateTime<Utc>, BackendState)> =
            backend.states.iter().cloned().collect();
        assert_eq!(
            state_vec,
            vec![
                (timestamp(1), BackendState::Starting),
                (timestamp(2), BackendState::Loading),
                (timestamp(3), BackendState::Ready),
                (timestamp(4), BackendState::Swept),
            ]
        );
    }
}

#[integration_test]
async fn repeated_backend_state_not_overwritten() {
    let fixture = StateTestFixture::new().await;
    let cluster = ClusterName::new("plane.test".into());
    let backend = BackendId::new_random();

    // Update the state of the backend to "starting".
    let result = apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Starting,
                    timestamp: timestamp(1),
                },
            }),
        },
    )
    .await
    .unwrap();

    assert!(result.is_some());

    {
        // Ensure that the the backend is in "starting" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(
            backend.state_timestamp().unwrap(),
            (timestamp(1), BackendState::Starting)
        );
    }

    let result = apply_state_message(
        &fixture.nats,
        &WorldStateMessage {
            cluster: cluster.clone(),
            message: ClusterStateMessage::BackendMessage(BackendMessage {
                backend: backend.clone(),
                message: BackendMessageType::State {
                    state: BackendState::Starting,
                    timestamp: timestamp(2),
                },
            }),
        },
    )
    .await
    .unwrap();

    assert!(result.is_none());

    {
        // Ensure that the the backend is in "starting" state.
        sleep(Duration::from_millis(100)).await;
        let state = fixture.state.state();
        let cluster = state.cluster(&cluster).unwrap();
        let backend = cluster.backend(&backend).unwrap();
        assert_eq!(
            backend.state_timestamp().unwrap(),
            (timestamp(1), BackendState::Starting)
        );
    }
}
