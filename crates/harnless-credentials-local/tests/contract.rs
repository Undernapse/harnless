//! Contract tests for the local credentials provider, one per acceptance
//! bullet of issue #14: rotation-without-restart and one-attempt-per-key
//! authorization.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use harnless_credentials_local::{AuthorizationFlow, AuthorizeState, LocalCredentials};
use harnless_seams::credentials::{CredentialKind, CredentialRef, Credentials};
use harnless_seams::error::ErrorCode;

fn cred(name: &str) -> CredentialRef {
    CredentialRef(name.to_string())
}

fn provider(dir: &std::path::Path) -> LocalCredentials {
    LocalCredentials::new(dir.join("credentials.json"))
}

#[tokio::test]
async fn authorize_runs_flow_and_stores_through_provider() {
    let dir = tempfile::tempdir().unwrap();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::OAuth2, |_reference| {
            Ok("flow-secret".to_string())
        })
        .build();
    let (secret, state) = creds
        .authorize(&cred("gh"), CredentialKind::OAuth2)
        .await
        .unwrap();
    assert_eq!(secret, "flow-secret");
    assert_eq!(state, AuthorizeState::Authorized);
    // The flow wrote the record *through the provider*: resolve sees it.
    assert_eq!(
        creds.resolve(&cred("gh")).unwrap().as_deref(),
        Some("flow-secret")
    );
    assert_eq!(
        creds.kind(&cred("gh")).unwrap(),
        Some(CredentialKind::OAuth2)
    );
}

#[tokio::test]
async fn unregistered_kind_is_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let creds = provider(dir.path());
    let err = creds
        .authorize(&cred("gh"), CredentialKind::OAuth2)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ToolDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_authorize_one_attempt_per_key() {
    // The registry obligation: two concurrent authorize calls for the same
    // key start exactly ONE dance; the second joins the first.
    let dir = tempfile::tempdir().unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let starts2 = starts.clone();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::OAuth2, move |_reference| {
            starts2.fetch_add(1, Ordering::SeqCst);
            // Long enough that the second call definitely overlaps.
            std::thread::sleep(std::time::Duration::from_millis(150));
            Ok("one-dance".to_string())
        })
        .build();
    let shared = cred("shared");
    let (a, b) = tokio::join!(
        creds.authorize(&shared, CredentialKind::OAuth2),
        creds.authorize(&shared, CredentialKind::OAuth2),
    );
    let (secret_a, state_a) = a.unwrap();
    let (secret_b, state_b) = b.unwrap();
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "second attempt must not start a second dance"
    );
    assert_eq!(secret_a, secret_b, "joiner adopts the runner's secret");
    assert_eq!(secret_a, "one-dance");
    // Exactly one runner, one joiner (order between the joined futures is
    // unspecified, so check the multiset).
    let states = [state_a, state_b];
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == AuthorizeState::Authorized)
            .count(),
        1,
        "exactly one call runs the dance: {states:?}"
    );
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == AuthorizeState::Joined)
            .count(),
        1,
        "the other call awaits and adopts: {states:?}"
    );
    assert_eq!(
        creds.resolve(&cred("shared")).unwrap().as_deref(),
        Some("one-dance")
    );
}

#[tokio::test]
async fn failed_flow_frees_key_for_later_attempt() {
    // One attempt per in-flight window: a failure is recorded, the key
    // frees, and a later attempt may run again.
    let dir = tempfile::tempdir().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts2 = attempts.clone();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::Bearer, move |_reference| {
            let n = attempts2.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                Err(harnless_seams::error::SeamError::new(
                    ErrorCode::ProviderFailure,
                    "user refused the dance",
                ))
            } else {
                Ok("second-try-secret".to_string())
            }
        })
        .build();
    let err = creds
        .authorize(&cred("retry"), CredentialKind::Bearer)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ProviderFailure);
    assert!(
        !creds.authorize_in_flight(&cred("retry")),
        "failed dance frees the key"
    );
    let (secret, state) = creds
        .authorize(&cred("retry"), CredentialKind::Bearer)
        .await
        .unwrap();
    assert_eq!(secret, "second-try-secret");
    assert_eq!(state, AuthorizeState::Authorized);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joiner_adopts_failure() {
    // A joiner of a failing dance gets the runner's typed error, not a
    // fabricated success.
    let dir = tempfile::tempdir().unwrap();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::ApiKey, move |_reference| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Err(harnless_seams::error::SeamError::code(
                ErrorCode::PermissionDenied,
            ))
        })
        .build();
    let doomed = cred("doomed");
    let (a, b) = tokio::join!(
        creds.authorize(&doomed, CredentialKind::ApiKey),
        creds.authorize(&doomed, CredentialKind::ApiKey),
    );
    for outcome in [a, b] {
        assert_eq!(outcome.unwrap_err().code, ErrorCode::PermissionDenied);
    }
    assert_eq!(
        creds.resolve(&cred("doomed")).unwrap(),
        None,
        "failed dance stores nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_runner_frees_key_and_wakes_joiners() {
    // A cancelled runner must not poison the key: the completion guard
    // publishes a typed failure and frees the slot on every exit, so a
    // joiner adopts the cancellation error and a later attempt may run.
    let dir = tempfile::tempdir().unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let starts2 = starts.clone();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::OAuth2, move |_reference| {
            starts2.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok("late".to_string())
        })
        .build();
    let key = cred("cancelled");
    let runner = tokio::spawn({
        let creds = creds.clone();
        let key = key.clone();
        async move { creds.authorize(&key, CredentialKind::OAuth2).await }
    });
    // Let the runner claim the key, then a joiner parks on it.
    while !creds.authorize_in_flight(&key) {
        tokio::task::yield_now().await;
    }
    let joiner = tokio::spawn({
        let creds = creds.clone();
        let key = key.clone();
        async move { creds.authorize(&key, CredentialKind::OAuth2).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    runner.abort();
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), joiner)
        .await
        .expect("joiner must wake after the runner is cancelled, not park forever")
        .expect("joiner task");
    assert_eq!(joined.unwrap_err().code, ErrorCode::IoError);
    assert!(
        !creds.authorize_in_flight(&key),
        "cancelled runner frees the key"
    );
    // A later attempt may run again (exactly one dance started so far).
    let (secret, state) = creds.authorize(&key, CredentialKind::OAuth2).await.unwrap();
    assert_eq!(secret, "late");
    assert_eq!(state, AuthorizeState::Authorized);
}

struct PromptFlow;

impl AuthorizationFlow for PromptFlow {
    fn authorize(&self, reference: &CredentialRef) -> harnless_seams::error::Result<String> {
        Ok(format!("secret-for-{}", reference.0))
    }
}

#[tokio::test]
async fn flow_impl_trait_object_registers_and_runs() {
    // The seam shape: an AuthorizationFlow implementor, registered per
    // kind, writes through the provider.
    let dir = tempfile::tempdir().unwrap();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow_impl(CredentialKind::Bearer, PromptFlow)
        .build();
    let (secret, _) = creds
        .authorize(&cred("svc"), CredentialKind::Bearer)
        .await
        .unwrap();
    assert_eq!(secret, "secret-for-svc");
    assert_eq!(
        creds.resolve(&cred("svc")).unwrap().as_deref(),
        Some("secret-for-svc")
    );
}

#[test]
fn flow_lifecycle_is_registrys_protocol_is_flows() {
    // Lifecycle pinning: the registry tracks in-flight state (in_flight
    // true during, false after) while never inspecting the secret's
    // provenance — any string the flow returns is stored verbatim.
    let dir = tempfile::tempdir().unwrap();
    let started = Arc::new(std::sync::Barrier::new(2));
    let started2 = started.clone();
    let inside = Arc::new(AtomicUsize::new(0));
    let inside2 = inside.clone();
    let creds = LocalCredentials::builder()
        .path(dir.path().join("credentials.json"))
        .flow(CredentialKind::OAuth2, move |_reference| {
            inside2.fetch_add(1, Ordering::SeqCst);
            started2.wait(); // hold the dance open until the test observed it
            Ok("verbatim\0weird\tsecret".to_string())
        })
        .build();
    let observer = creds.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(creds.authorize(&cred("lifecycle"), CredentialKind::OAuth2))
    });
    // Wait until the dance is in flight, then assert lifecycle state.
    loop {
        if inside.load(Ordering::SeqCst) == 1 && observer.authorize_in_flight(&cred("lifecycle")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    started.wait();
    let (secret, _) = handle.join().unwrap().unwrap();
    assert_eq!(
        secret, "verbatim\0weird\tsecret",
        "flow protocol output is stored as-is"
    );
    assert!(
        !observer.authorize_in_flight(&cred("lifecycle")),
        "lifecycle cleanup on completion"
    );
}
