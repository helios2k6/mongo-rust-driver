pub(crate) mod event;
mod file;
mod integration;

use std::{collections::HashMap, ops::Deref, sync::Arc, time::Duration};

use tokio::sync::{Mutex, RwLock, RwLockWriteGuard};

use self::{
    event::{Event, EventHandler},
    file::{Operation, TestFile, ThreadedOperation},
};

use crate::{
    cmap::{Connection, ConnectionPool, ConnectionPoolOptions},
    error::{Error, ErrorKind, Result},
    event::cmap::ConnectionPoolOptions as EventOptions,
    options::TlsOptions,
    runtime,
    runtime::AsyncJoinHandle,
    sdam::{TopologyUpdater, UpdateMessage},
    test::{
        assert_matches,
        eq_matches,
        log_uncaptured,
        run_spec_test,
        EventClient,
        MatchErrExt,
        Matchable,
        CLIENT_OPTIONS,
        LOCK,
        SERVER_API,
    },
};
use bson::doc;

const TEST_DESCRIPTIONS_TO_SKIP: &[&str] = &[
    "must destroy checked in connection if pool has been closed",
    "must throw error if checkOut is called on a closed pool",
    // WaitQueueTimeoutMS is not supported
    "must aggressively timeout threads enqueued longer than waitQueueTimeoutMS",
    "waiting on maxConnecting is limited by WaitQueueTimeoutMS",
    // TODO DRIVERS-1785 remove this skip when test event order is fixed
    "error during minPoolSize population clears pool",
];

/// Many different types of CMAP events are emitted from tasks spawned in the drop
/// implementations of various types (Connections, pools, etc.). Sometimes it takes
/// a longer amount of time for these tasks to get scheduled and thus their associated
/// events to get emitted, requiring the runner to wait for a little bit before asserting
/// the events were actually fired.
///
/// This value was purposefully chosen to be large to prevent test failures, though it is not
/// expected that the 3s timeout will regularly or ever be hit.
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct Executor {
    description: String,
    operations: Vec<ThreadedOperation>,
    error: Option<self::file::Error>,
    events: Vec<Event>,
    state: Arc<State>,
    ignored_event_names: Vec<String>,
    pool_options: ConnectionPoolOptions,
}

#[derive(Debug)]
struct State {
    handler: Arc<EventHandler>,
    connections: RwLock<HashMap<String, Connection>>,
    unlabeled_connections: Mutex<Vec<Connection>>,
    threads: RwLock<HashMap<String, CmapThread>>,

    // In order to drop the pool when performing a `close` operation, we use an `Option` so that we
    // can replace it with `None`. Since none of the tests should use the pool after its closed
    // (besides the ones we manually skip over), it's fine for us to `unwrap` the pool during these
    // tests, as panicking is sufficient to exit any aberrant test with a failure.
    pool: RwLock<Option<ConnectionPool>>,
}

impl State {
    // Counts the number of events of the given type that have occurred so far.
    fn count_events(&self, event_type: &str) -> usize {
        self.handler
            .events
            .read()
            .unwrap()
            .iter()
            .filter(|cmap_event| cmap_event.name() == event_type)
            .count()
    }
}

#[derive(Debug)]
struct CmapThread {
    handle: AsyncJoinHandle<Result<()>>,
    dispatcher: tokio::sync::mpsc::UnboundedSender<Operation>,
}

impl CmapThread {
    fn start(state: Arc<State>) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Operation>();
        let handle = runtime::spawn(async move {
            while let Some(operation) = receiver.recv().await {
                operation.execute(state.clone()).await?;
            }
            Ok(())
        });

        Self {
            dispatcher: sender,
            handle,
        }
    }

    async fn wait_until_complete(self) -> Result<()> {
        // hang up dispatcher so task will complete.
        drop(self.dispatcher);
        self.handle.await
    }
}

impl Executor {
    fn new(test_file: TestFile) -> Self {
        let handler = Arc::new(EventHandler::new());
        let error = test_file.error;

        let mut pool_options = test_file.pool_options.unwrap_or_default();
        pool_options.tls_options = CLIENT_OPTIONS.tls_options();
        pool_options.cmap_event_handler = Some(handler.clone());
        pool_options.server_api = SERVER_API.clone();

        let state = State {
            handler,
            pool: RwLock::new(None),
            connections: Default::default(),
            threads: Default::default(),
            unlabeled_connections: Mutex::new(Default::default()),
        };

        Self {
            description: test_file.description,
            error,
            events: test_file.events,
            operations: test_file.operations,
            state: Arc::new(state),
            ignored_event_names: test_file.ignore,
            pool_options,
        }
    }

    async fn execute_test(self) {
        let mut subscriber = self.state.handler.subscribe();

        let (updater, mut receiver) = TopologyUpdater::channel();

        let pool = ConnectionPool::new(
            CLIENT_OPTIONS.hosts[0].clone(),
            Default::default(),
            updater,
            Some(self.pool_options),
        );

        // Mock a monitoring task responding to errors reported by the pool.
        let manager = pool.manager.clone();
        runtime::execute(async move {
            while let Some(update) = receiver.recv().await {
                let (update, ack) = update.into_parts();
                if let UpdateMessage::ApplicationError { error, .. } = update {
                    manager.clear(error, None).await;
                }
                ack.acknowledge(true);
            }
        });

        *self.state.pool.write().await = Some(pool);

        let mut error: Option<Error> = None;
        let operations = self.operations;

        println!("Executing {}", self.description);

        for operation in operations {
            let err = operation.execute(self.state.clone()).await.err();
            if error.is_none() {
                error = err;
            }
        }

        match (self.error, error) {
            (Some(ref expected), None) => {
                panic!("Expected {}, but no error occurred", expected.type_)
            }
            (None, Some(ref actual)) => panic!(
                "Expected no error to occur, but the following error was returned: {:?}",
                actual
            ),
            (None, None) | (Some(_), Some(_)) => {}
        }

        let ignored_event_names = self.ignored_event_names;
        let description = self.description;
        let filter = |e: &Event| !ignored_event_names.iter().any(|name| e.name() == name);
        for expected_event in self.events {
            let actual_event = subscriber
                .wait_for_event(EVENT_TIMEOUT, filter)
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "{}: did not receive expected event: {:?}",
                        description, expected_event
                    )
                });
            assert_matches(&actual_event, &expected_event, Some(description.as_str()));
        }

        assert_eq!(subscriber.all(filter), Vec::new(), "{}", description);
    }
}

impl Operation {
    /// Execute this operation.
    async fn execute(self, state: Arc<State>) -> Result<()> {
        match self {
            Operation::Wait { ms } => runtime::delay_for(Duration::from_millis(ms)).await,
            Operation::WaitForThread { target } => {
                state
                    .threads
                    .write()
                    .await
                    .remove(&target)
                    .unwrap()
                    .wait_until_complete()
                    .await?
            }
            Operation::WaitForEvent {
                event,
                count,
                timeout,
            } => {
                let event_name = event.clone();
                let task = async move {
                    while state.count_events(&event) < count {
                        runtime::delay_for(Duration::from_millis(100)).await;
                    }
                };
                runtime::timeout(timeout.unwrap_or(EVENT_TIMEOUT), task)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("waiting for {} {} event(s) timed out", count, event_name)
                    });
            }
            Operation::CheckOut { label } => {
                if let Some(pool) = state.pool.read().await.deref() {
                    let conn = pool.check_out().await?;

                    if let Some(label) = label {
                        state.connections.write().await.insert(label, conn);
                    } else {
                        state.unlabeled_connections.lock().await.push(conn);
                    }
                }
            }
            Operation::CheckIn { connection } => {
                let mut subscriber = state.handler.subscribe();
                let conn = state.connections.write().await.remove(&connection).unwrap();
                let id = conn.id;
                // connections are checked in via tasks spawned in their drop implementation,
                // they are not checked in explicitly.
                drop(conn);

                // wait for event to be emitted to ensure check in has completed.
                subscriber
                    .wait_for_event(EVENT_TIMEOUT, |e| {
                        matches!(e, Event::ConnectionCheckedIn(event) if event.connection_id == id)
                    })
                    .await
                    .unwrap_or_else(|| {
                        panic!(
                            "did not receive checkin event after dropping connection (id={})",
                            connection
                        )
                    });
            }
            Operation::Clear => {
                if let Some(pool) = state.pool.read().await.as_ref() {
                    pool.clear(
                        ErrorKind::Internal {
                            message: "test error".to_string(),
                        }
                        .into(),
                        None,
                    )
                    .await;
                }
            }
            Operation::Ready => {
                if let Some(pool) = state.pool.read().await.deref() {
                    pool.mark_as_ready().await;
                }
            }
            Operation::Close => {
                let mut subscriber = state.handler.subscribe();

                // pools are closed via their drop implementation
                state.pool.write().await.take();

                // wait for event to be emitted to ensure drop has completed.
                subscriber
                    .wait_for_event(EVENT_TIMEOUT, |e| matches!(e, Event::PoolClosed(_)))
                    .await
                    .expect("did not receive ConnectionPoolClosed event after closing pool");
            }
            Operation::Start { target } => {
                state
                    .threads
                    .write()
                    .await
                    .insert(target, CmapThread::start(state.clone()));
            }
        }
        Ok(())
    }
}

impl Matchable for TlsOptions {
    fn content_matches(&self, expected: &TlsOptions) -> std::result::Result<(), String> {
        self.allow_invalid_certificates
            .matches(&expected.allow_invalid_certificates)
            .prefix("allow_invalid_certificates")?;
        self.ca_file_path
            .as_ref()
            .map(|pb| pb.display().to_string())
            .matches(
                &expected
                    .ca_file_path
                    .as_ref()
                    .map(|pb| pb.display().to_string()),
            )
            .prefix("ca_file_path")?;
        self.cert_key_file_path
            .as_ref()
            .map(|pb| pb.display().to_string())
            .matches(
                &expected
                    .cert_key_file_path
                    .as_ref()
                    .map(|pb| pb.display().to_string()),
            )
            .prefix("cert_key_file_path")?;
        Ok(())
    }
}

impl Matchable for EventOptions {
    fn content_matches(&self, expected: &EventOptions) -> std::result::Result<(), String> {
        self.max_idle_time
            .matches(&expected.max_idle_time)
            .prefix("max_idle_time")?;
        self.max_pool_size
            .matches(&expected.max_pool_size)
            .prefix("max_pool_size")?;
        self.min_pool_size
            .matches(&expected.min_pool_size)
            .prefix("min_pool_size")?;
        Ok(())
    }
}

impl Matchable for Event {
    fn content_matches(&self, expected: &Event) -> std::result::Result<(), String> {
        match (self, expected) {
            (Event::PoolCreated(actual), Event::PoolCreated(ref expected)) => {
                actual.options.matches(&expected.options)
            }
            (Event::ConnectionCreated(actual), Event::ConnectionCreated(ref expected)) => {
                actual.connection_id.matches(&expected.connection_id)
            }
            (Event::ConnectionReady(actual), Event::ConnectionReady(ref expected)) => {
                actual.connection_id.matches(&expected.connection_id)
            }
            (Event::ConnectionClosed(actual), Event::ConnectionClosed(ref expected)) => {
                eq_matches("reason", &actual.reason, &expected.reason)?;
                actual
                    .connection_id
                    .matches(&expected.connection_id)
                    .prefix("connection_id")?;
                Ok(())
            }
            (Event::ConnectionCheckedOut(actual), Event::ConnectionCheckedOut(ref expected)) => {
                actual.connection_id.matches(&expected.connection_id)
            }
            (Event::ConnectionCheckedIn(actual), Event::ConnectionCheckedIn(ref expected)) => {
                actual.connection_id.matches(&expected.connection_id)
            }
            (
                Event::ConnectionCheckOutFailed(actual),
                Event::ConnectionCheckOutFailed(ref expected),
            ) => {
                if actual.reason == expected.reason {
                    Ok(())
                } else {
                    Err(format!(
                        "expected reason {:?}, got {:?}",
                        expected.reason, actual.reason
                    ))
                }
            }
            (Event::ConnectionCheckOutStarted(_), Event::ConnectionCheckOutStarted(_)) => Ok(()),
            (Event::PoolCleared(_), Event::PoolCleared(_)) => Ok(()),
            (Event::PoolReady(_), Event::PoolReady(_)) => Ok(()),
            (Event::PoolClosed(_), Event::PoolClosed(_)) => Ok(()),
            (actual, expected) => Err(format!("expected event {:?}, got {:?}", actual, expected)),
        }
    }
}

#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn cmap_spec_tests() {
    async fn run_cmap_spec_tests(test_file: TestFile) {
        if TEST_DESCRIPTIONS_TO_SKIP.contains(&test_file.description.as_str()) {
            return;
        }

        let _guard: RwLockWriteGuard<()> = LOCK.run_exclusively().await;

        let mut options = CLIENT_OPTIONS.clone();
        if options.load_balanced.unwrap_or(false) {
            log_uncaptured(format!(
                "skipping {:?} due to load balanced topology",
                test_file.description
            ));
            return;
        }
        options.hosts.drain(1..);
        options.direct_connection = Some(true);
        let client = EventClient::with_options(options).await;
        if let Some(ref run_on) = test_file.run_on {
            let can_run_on = run_on.iter().any(|run_on| run_on.can_run_on(&client));
            if !can_run_on {
                log_uncaptured("skipping due to runOn requirements");
                return;
            }
        }

        let should_disable_fp = test_file.fail_point.is_some();
        if let Some(ref fail_point) = test_file.fail_point {
            client
                .database("admin")
                .run_command(fail_point.clone(), None)
                .await
                .unwrap();
        }

        let executor = Executor::new(test_file);
        executor.execute_test().await;

        if should_disable_fp {
            client
                .database("admin")
                .run_command(
                    doc! {
                        "configureFailPoint": "failCommand",
                        "mode": "off"
                    },
                    None,
                )
                .await
                .unwrap();
        }
    }

    run_spec_test(&["connection-monitoring-and-pooling"], run_cmap_spec_tests).await;
}
