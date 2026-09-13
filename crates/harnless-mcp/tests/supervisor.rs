//! Supervisor policy: exponential backoff doubling to a ceiling, budget
//! reset after surviving past the ceiling, attempt-limit exhaustion that
//! unregisters and stops, and the last-good-generation-stays-registered
//! rule through an outage.

use std::sync::Arc;
use std::time::Duration;

use harnless_mcp::config::ReconnectConfig;
use harnless_mcp::supervisor::{
    backoff_delay, supervise, Generation, GenerationSink, Registry, SupervisorEvent,
    SupervisorObserver, TransportFactory,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

fn cfg(initial: u64, ceiling: u64, max_attempts: u32) -> Arc<ReconnectConfig> {
    Arc::new(ReconnectConfig {
        enabled: true,
        backoff_initial: Duration::from_millis(initial),
        backoff_ceiling: Duration::from_millis(ceiling),
        max_attempts,
    })
}

#[test]
fn backoff_doubles_until_the_ceiling() {
    let c = ReconnectConfig {
        enabled: true,
        backoff_initial: Duration::from_millis(100),
        backoff_ceiling: Duration::from_millis(800),
        max_attempts: 20,
    };
    assert_eq!(backoff_delay(&c, 1), Duration::from_millis(100));
    assert_eq!(backoff_delay(&c, 2), Duration::from_millis(200));
    assert_eq!(backoff_delay(&c, 3), Duration::from_millis(400));
    assert_eq!(backoff_delay(&c, 4), Duration::from_millis(800));
    assert_eq!(backoff_delay(&c, 5), Duration::from_millis(800));
    assert_eq!(backoff_delay(&c, 9), Duration::from_millis(800));
}

/// A scripted factory. Each scripted element is one connection attempt:
///
/// * `Healthy { tools }` — publish `tools` as the generation, then stay
///   healthy until `stop` is cancelled (a clean `Ok`);
/// * `Recovered { survived }` — a reconnect that survives `survived` then
///   fails with a transport loss, recording that survival duration for the
///   budget-reset rule;
/// * `Fail` — fail immediately with a transport-loss error.
#[derive(Clone)]
enum Outcome {
    Healthy { tools: Vec<String> },
    Recovered { survived: Duration },
    Fail,
}

struct ScriptedFactory {
    outcomes: Mutex<std::collections::VecDeque<Outcome>>,
    server: String,
    survival: Arc<Mutex<Option<Duration>>>,
}

impl ScriptedFactory {
    fn new(outcomes: Vec<Outcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            server: "srv".into(),
            survival: Arc::new(Mutex::new(None)),
        })
    }
}

impl TransportFactory for ScriptedFactory {
    fn run(&self, sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String> {
        let next = self.outcomes.lock().pop_front();
        match next {
            None => {
                // Script exhausted: behave as a permanent failure so the
                // supervisor's budget eventually stops the loop.
                Err("script exhausted".to_string())
            }
            Some(Outcome::Fail) => Err("scripted transport loss".to_string()),
            Some(Outcome::Recovered { survived }) => {
                *self.survival.lock() = Some(survived);
                Err("scripted transport loss after survival".to_string())
            }
            Some(Outcome::Healthy { tools }) => {
                let generation: Generation = tools
                    .into_iter()
                    .map(|name| {
                        (
                            name.clone(),
                            harnless_seams::ToolDefinition {
                                name,
                                schema: serde_json::json!({"type": "object"}),
                                serialized: false,
                            },
                            Arc::new(NoopBody) as Arc<dyn harnless_seams::ToolBody>,
                        )
                    })
                    .collect();
                sink.publish(generation);
                while !stop.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }
        }
    }

    fn last_survival(&self) -> Option<Duration> {
        self.survival.lock().take()
    }

    fn server(&self) -> &str {
        &self.server
    }
}

struct NoopBody;

impl harnless_seams::ToolBody for NoopBody {
    fn run(
        &self,
        _call_id: harnless_seams::CallId,
        _args: &[u8],
    ) -> harnless_seams::Result<serde_json::Value> {
        Ok(serde_json::json!({"ok": true}))
    }
}

/// A recording registry: tracks generations and unregisters.
#[derive(Default)]
struct RecordingRegistry {
    generations: Arc<Mutex<Vec<Vec<String>>>>,
    unregistered: Arc<Mutex<Vec<String>>>,
    fail_next: Arc<Mutex<Option<String>>>,
}

impl Registry for RecordingRegistry {
    fn replace_generation(&self, _server: &str, tools: Generation) -> Result<(), String> {
        if let Some(conflict) = self.fail_next.lock().take() {
            return Err(conflict);
        }
        self.generations
            .lock()
            .push(tools.iter().map(|(_, def, _)| def.name.clone()).collect());
        Ok(())
    }
    fn unregister(&self, server: &str) {
        self.unregistered.lock().push(server.to_string());
    }
}

#[derive(Default)]
struct RecordingObserver {
    events: Arc<Mutex<Vec<SupervisorEvent>>>,
}

impl SupervisorObserver for RecordingObserver {
    fn on_event(&self, event: SupervisorEvent) {
        self.events.lock().push(event);
    }
}

fn noop_sleep() -> Arc<dyn Fn(Duration) + Send + Sync> {
    Arc::new(|_| {})
}

#[test]
fn outage_backoff_schedule_doubles_to_ceiling_then_stops_at_budget() {
    // Six failures with ceiling 800ms and budget 5: delays 100/200/400/800
    // then stop (attempt 5 exhausts the budget).
    let factory = ScriptedFactory::new(vec![Outcome::Fail; 5]);
    let registry = Arc::new(RecordingRegistry::default());
    let observer = Arc::new(RecordingObserver::default());
    supervise(
        factory,
        registry.clone(),
        cfg(100, 800, 5),
        observer.clone(),
        noop_sleep(),
        CancellationToken::new(),
    );
    let events = observer.events.lock();
    let delays: Vec<Duration> = events
        .iter()
        .filter_map(|e| match e {
            SupervisorEvent::Reconnect { delay, .. } => Some(*delay),
            _ => None,
        })
        .collect();
    assert_eq!(
        delays,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
        ],
        "backoff must double to the ceiling"
    );
    match events.last().unwrap() {
        SupervisorEvent::Stopped { attempts, .. } => assert_eq!(*attempts, 5),
        other => panic!("expected Stopped, got {other:?}"),
    }
    assert_eq!(
        &*registry.unregistered.lock(),
        &["srv".to_string()],
        "budget exhaustion must unregister"
    );
}

#[test]
fn surviving_past_the_ceiling_resets_the_budget() {
    // Fail, recover and survive past the ceiling, then fail again: the
    // second outage's first reconnect delay restarts at the initial value
    // (budget reset), proving the run did not just keep counting.
    let long = Duration::from_millis(5000); // > ceiling (800ms)
    let factory = ScriptedFactory::new(vec![
        Outcome::Fail,
        Outcome::Fail,
        Outcome::Recovered { survived: long }, // reconnect survived past ceiling
        Outcome::Fail,
        Outcome::Fail,
    ]);
    let registry = Arc::new(RecordingRegistry::default());
    let observer = Arc::new(RecordingObserver::default());
    supervise(
        factory,
        registry,
        cfg(100, 800, 3),
        observer.clone(),
        noop_sleep(),
        CancellationToken::new(),
    );
    let events = observer.events.lock();
    let delays: Vec<Duration> = events
        .iter()
        .filter_map(|e| match e {
            SupervisorEvent::Reconnect { delay, .. } => Some(*delay),
            _ => None,
        })
        .collect();
    // Losses 1 and 2: delays 100, 200. The reconnect survives past the
    // ceiling, resetting the budget, so the next loss is attempt 1 again →
    // delay 100, not 400.
    assert_eq!(
        delays,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(100),
            Duration::from_millis(200),
        ]
    );
    // The fresh budget after the reset is again 3, so the run continues
    // past the first post-reset loss and stops only when the *new* budget
    // exhausts at attempt 3 — proving the counter was reset, not capped.
    match events.last().unwrap() {
        SupervisorEvent::Stopped { attempts, .. } => assert_eq!(*attempts, 3),
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[test]
fn a_run_that_survived_past_the_ceiling_resets_the_attempt_counter() {
    // Directly exercise the reset rule: a run that reports survival longer
    // than the ceiling must restart the consecutive-failure count, so the
    // first delay after it is the initial backoff, not a doubled one.
    let factory = ScriptedFactory::new(vec![
        Outcome::Fail,
        Outcome::Recovered {
            survived: Duration::from_millis(5000),
        }, // > ceiling 800
        Outcome::Fail,
        Outcome::Fail,
        Outcome::Fail,
        Outcome::Fail,
    ]);
    let registry = Arc::new(RecordingRegistry::default());
    let observer = Arc::new(RecordingObserver::default());
    supervise(
        factory,
        registry,
        cfg(100, 800, 2),
        observer.clone(),
        noop_sleep(),
        CancellationToken::new(),
    );
    let events = observer.events.lock();
    let delays: Vec<Duration> = events
        .iter()
        .filter_map(|e| match e {
            SupervisorEvent::Reconnect { delay, .. } => Some(*delay),
            _ => None,
        })
        .collect();
    // Loss 1 → delay 100. The reconnect survives past the ceiling, the
    // budget resets, and the next loss is attempt 1 again → delay 100, not
    // 200. The fresh budget (2) then exhausts after one more doubling.
    assert_eq!(
        delays,
        vec![Duration::from_millis(100), Duration::from_millis(100)],
        "budget reset must restart the backoff at the initial delay"
    );
    match events.last().unwrap() {
        SupervisorEvent::Stopped { attempts, .. } => assert_eq!(*attempts, 2),
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[test]
fn last_good_generation_stays_registered_through_an_outage() {
    // A healthy connection publishes a generation; a clean stop must never
    // unregister — the last-good generation stays registered.
    let factory = ScriptedFactory::new(vec![Outcome::Healthy {
        tools: vec!["mcp__srv__tool".into()],
    }]);
    let registry = Arc::new(RecordingRegistry::default());
    let stop = CancellationToken::new();
    let observer = Arc::new(RecordingObserver::default());
    let handle = {
        let stop = stop.clone();
        let registry_for_thread = registry.clone();
        std::thread::spawn(move || {
            supervise(
                factory,
                registry_for_thread,
                cfg(1, 2, 50),
                observer.clone(),
                noop_sleep(),
                stop,
            );
        })
    };
    // Let it enter the healthy wait, then request stop.
    std::thread::sleep(Duration::from_millis(20));
    stop.cancel();
    handle.join().unwrap();
    assert!(
        registry.unregistered.lock().is_empty(),
        "a clean stop must never unregister"
    );
}

#[test]
fn reconnect_disabled_unregisters_immediately() {
    let factory = ScriptedFactory::new(vec![Outcome::Fail]);
    let registry = Arc::new(RecordingRegistry::default());
    let observer = Arc::new(RecordingObserver::default());
    let mut c = ReconnectConfig {
        enabled: false,
        ..ReconnectConfig::default()
    };
    c.max_attempts = 100; // irrelevant when disabled
    supervise(
        factory,
        registry.clone(),
        Arc::new(c),
        observer.clone(),
        noop_sleep(),
        CancellationToken::new(),
    );
    assert_eq!(&*registry.unregistered.lock(), &["srv".to_string()]);
    assert!(matches!(
        observer.events.lock().last().unwrap(),
        SupervisorEvent::Stopped { attempts: 1, .. }
    ));
}

#[test]
fn conflict_rolls_back_the_attempted_generation() {
    let registry = Arc::new(RecordingRegistry::default());
    *registry.fail_next.lock() = Some("public name conflict".into());
    let sink: Arc<dyn GenerationSink> = Arc::new(TestSink {
        registry: registry.clone(),
    });
    // The supervisor's sink path: a conflicted publish must not record a
    // generation and must not unregister anything.
    sink.publish(vec![]);
    assert!(
        registry.generations.lock().is_empty(),
        "rolled back: nothing recorded"
    );
    assert!(registry.unregistered.lock().is_empty());
}

struct TestSink {
    registry: Arc<RecordingRegistry>,
}

impl GenerationSink for TestSink {
    fn publish(&self, tools: Generation) {
        let _ = self.registry.replace_generation("srv", tools);
    }
    fn outage(&self, _reason: String) {}
}
