//! Test-support: an in-memory fake MCP server for the primary seam.
//!
//! The fake speaks real JSON-RPC 2.0 over a channel-backed transport pair,
//! so the bridge's production rmcp path is exercised end to end with no
//! network and no child process. Tests script responses through a
//! [`FakeServer`] and drive list-changed notifications and transport loss.

use std::sync::Arc;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use rmcp::model::{
    CallToolResult, ErrorData, JsonRpcNotification, JsonRpcResponse, JsonRpcVersion2_0,
    ListToolsResult, ServerNotification, ServerRequest, ServerResult, Tool,
};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use serde_json::{json, Value};

/// A decoded request the fake received.
#[derive(Debug, Clone)]
pub struct Request {
    /// JSON-RPC request id (raw JSON so any spelling round-trips).
    pub id: Value,
    /// Method name (`initialize`, `tools/list`, `tools/call`, …).
    pub method: String,
    /// Raw params payload.
    pub params: Value,
}

/// A canned response to a request.
#[derive(Debug, Clone)]
pub enum Response {
    /// A success result (raw JSON placed under `result`).
    Result(Value),
    /// A JSON-RPC error.
    Error {
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
    },
}

impl Response {
    /// A successful `initialize` handshake.
    pub fn initialize(tools_list_changed: bool) -> Self {
        Self::Result(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": { "listChanged": tools_list_changed } },
            "serverInfo": { "name": "fake-mcp", "version": "0.1.0" },
        }))
    }

    /// A successful `tools/list` page.
    pub fn tools(tools: Vec<Tool>) -> Self {
        Self::Result(serde_json::to_value(ListToolsResult::with_all_items(tools)).unwrap())
    }

    /// A successful `tools/call` result.
    pub fn call(result: CallToolResult) -> Self {
        Self::Result(serde_json::to_value(result).unwrap())
    }
}

/// A tool definition shorthand for tests.
pub fn tool(name: &str) -> Tool {
    Tool::new(
        name.to_string(),
        format!("the {name} tool"),
        serde_json::Map::from_iter(
            (json!({ "type": "object", "properties": {} }))
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
}

/// A scripted responder: answers each decoded request (or stays silent).
pub type Responder = Arc<dyn Fn(&Request) -> Option<Response> + Send + Sync + 'static>;

/// The fake server: a transport pair the bridge can serve against, plus
/// controls to push notifications and kill the transport.
pub struct FakeServer {
    /// Requests the fake answered, in order.
    pub seen: Arc<Mutex<Vec<Request>>>,
    /// The notification channel, so tests can push list_changed at a live
    /// connection.
    pub notify_tx: mpsc::Sender<ServerNotification>,
    /// The kill switch, so tests can drop the transport from anywhere.
    pub kill: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// Resolves once the fake has answered its first `tools/list`.
    pub listed: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    /// The transport half handed to the bridge.
    pub transport: FakeTransport,
}

/// The transport pair the bridge serves against.
pub type FakeTransport = (
    std::pin::Pin<
        Box<
            dyn futures::Sink<
                    TxJsonRpcMessage<rmcp::RoleClient>,
                    Error = crate::supervisor::TransportPumpError,
                > + Send,
        >,
    >,
    futures::stream::BoxStream<'static, RxJsonRpcMessage<rmcp::RoleClient>>,
);

/// Build a fake server whose responder is `responder`.
// scripted_tools helper
pub fn scripted_tools(names: &[&str]) -> Vec<Tool> {
    names.iter().map(|n| tool(n)).collect()
}

pub fn fake_server(responder: Responder) -> FakeServer {
    // Bridge → fake (requests/responses from the client side).
    let (to_fake_tx, mut to_fake_rx) = mpsc::channel::<TxJsonRpcMessage<rmcp::RoleClient>>(64);
    // Fake → bridge (responses/notifications from the server side).
    let (to_bridge_tx, to_bridge_rx) = mpsc::channel::<RxJsonRpcMessage<rmcp::RoleClient>>(64);
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let (notify_tx, mut notify_rx) = mpsc::channel::<ServerNotification>(16);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let done_tx = Arc::new(std::sync::Mutex::new(Some(done_tx)));
    // Pump: decode client messages, answer via responder; forward
    // notifications; end the stream on kill or channel close. Runs on the
    // crate's shared runtime so the fake works from any thread context.
    let _guard = crate::bridge::runtime().enter();
    tokio::spawn(async move {
        let responder = responder;
        let mut wire_tx = to_bridge_tx;
        let mut kill_rx = std::pin::pin!(kill_rx);
        loop {
            tokio::select! {
                _ = &mut kill_rx => break,
                n = notify_rx.next() => {
                    if let Some(n) = n {
                        if wire_tx.send(remap_wire(&ServerJsonRpcWire::Notification(n))).await.is_err() {
                            break;
                        }
                    }
                }
                msg = to_fake_rx.next() => {
                    let Some(msg) = msg else { break };
                    let Some((id, req)) = decode_client_request(&msg) else { continue };
                    let decoded = Request {
                        id: serde_json::to_value(&id).unwrap_or(Value::Null),
                        method: method_of(&req),
                        params: params_of(&req),
                    };
                    seen2.lock().push(decoded.clone());
                    if decoded.method == "tools/list" {
                        if let Some(tx) = done_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                    }
                    if let Some(answer) = (responder)(&decoded) {
                        let out = match answer {
                            Response::Result(value) => ServerJsonRpcWire::Response {
                                id: id.clone(),
                                result: value,
                            },
                            Response::Error { code, message } => ServerJsonRpcWire::Error {
                                id: id.clone(),
                                code,
                                message,
                            },
                        };
                        if wire_tx.send(remap_wire(&out)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    let sink = futures::sink::unfold(to_fake_tx, |mut tx, msg| async move {
        tx.send(msg)
            .await
            .map(|_| tx)
            .map_err(|e| crate::supervisor::TransportPumpError(e.to_string()))
    });
    FakeServer {
        seen,
        notify_tx,
        kill: Arc::new(Mutex::new(Some(kill_tx))),
        listed: Arc::new(std::sync::Mutex::new(Some(done_rx))),
        transport: (Box::pin(sink), Box::pin(to_bridge_rx)),
    }
}

impl FakeServer {
    /// Push a `notifications/tools/list_changed` at the bridge.
    pub async fn fire_list_changed(&mut self) {
        let _ = self
            .notify_tx
            .try_send(ServerNotification::ToolListChangedNotification(
                rmcp::model::NotificationNoParam::default(),
            ));
    }

    /// Kill the transport: the bridge observes end-of-stream (transport
    /// loss) exactly like a dead child process.
    pub fn kill(&self) {
        if let Some(tx) = self.kill.lock().take() {
            let _ = tx.send(());
        }
    }
}

/// The wire-level server message the fake produces, in raw JSON form so
/// the responder never has to name a Rust variant.
enum ServerJsonRpcWire {
    Response {
        id: rmcp::model::RequestId,
        result: Value,
    },
    Error {
        id: rmcp::model::RequestId,
        code: i64,
        message: String,
    },
    Notification(ServerNotification),
}

/// Remap our raw wire form into the concrete typed message rmcp expects.
fn remap_wire(msg: &ServerJsonRpcWire) -> RxJsonRpcMessage<rmcp::RoleClient> {
    match msg {
        ServerJsonRpcWire::Response { id, result } => {
            let result: ServerResult = serde_json::from_value(wrapped_result(result))
                .expect("fake produced an unparseable result");
            RxJsonRpcMessage::<rmcp::RoleClient>::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion2_0,
                id: id.clone(),
                result,
            })
        }
        ServerJsonRpcWire::Error { id, code, message } => {
            RxJsonRpcMessage::<rmcp::RoleClient>::Error(rmcp::model::JsonRpcError {
                jsonrpc: JsonRpcVersion2_0,
                id: Some(id.clone()),
                error: ErrorData::new(rmcp::model::ErrorCode(*code as i32), message.clone(), None),
            })
        }
        ServerJsonRpcWire::Notification(n) => {
            RxJsonRpcMessage::<rmcp::RoleClient>::Notification(JsonRpcNotification {
                jsonrpc: JsonRpcVersion2_0,
                notification: n.clone(),
            })
        }
    }
}

/// rmcp's `ServerResult` is an untagged union; a raw result JSON object
/// deserializes into the matching variant directly, so wrap nothing — but
/// guard against the empty-object case which matches no variant.
fn wrapped_result(result: &Value) -> Value {
    if result.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        // An empty result object matches `EmptyResult` only with the
        // `{}` spelling rmcp accepts.
        return json!({});
    }
    result.clone()
}

/// Decode a client-side outgoing message into the request the fake must
/// answer. Notifications and responses from the client are ignored.
fn decode_client_request(
    msg: &TxJsonRpcMessage<rmcp::RoleClient>,
) -> Option<(rmcp::model::RequestId, ServerRequest)> {
    // The client's outgoing request arrives as a `ClientJsonRpcMessage::Request`;
    // the fake needs its id, method, and params as raw JSON.
    let wire = serde_json::to_value(msg).ok()?;
    if wire.get("method").and_then(Value::as_str)?.is_empty() {
        return None;
    }
    let id = wire.get("id")?.clone();
    let params = wire.get("params").cloned().unwrap_or(Value::Null);
    let method = wire
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Rebuild a ServerRequest-shaped value purely for method/params
    // extraction (the fake never downcasts it).
    let req: ServerRequest = serde_json::from_value(json!({
        "method": method,
        "params": params,
    }))
    .unwrap_or_else(|_| {
        // CustomRequest fallback keeps the method name.
        ServerRequest::CustomRequest(rmcp::model::CustomRequest::new(method, Some(params)))
    });
    let id: rmcp::model::RequestId =
        serde_json::from_value(id).unwrap_or(rmcp::model::NumberOrString::Number(0));
    Some((id, req))
}

fn method_of(req: &ServerRequest) -> String {
    match req {
        ServerRequest::PingRequest(_) => "ping".to_string(),
        ServerRequest::CreateMessageRequest(_) => "sampling/createMessage".to_string(),
        ServerRequest::ListRootsRequest(_) => "roots/list".to_string(),
        ServerRequest::ElicitRequest(_) => "elicitation/create".to_string(),
        ServerRequest::CustomRequest(c) => c.method.clone(),
    }
}

fn params_of(req: &ServerRequest) -> Value {
    match req {
        ServerRequest::CustomRequest(c) => c.params.clone().unwrap_or(Value::Null),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.get("params").cloned())
            .unwrap_or(Value::Null),
    }
}

/// A scripted responder answering a fixed handshake + tool list, with a
/// `tools/call` handler closure supplied by the test.
pub fn scripted(
    tools: Vec<Tool>,
    call_handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
) -> Responder {
    Arc::new(move |req: &Request| match req.method.as_str() {
        "initialize" => Some(Response::initialize(true)),
        "tools/list" => Some(Response::tools(tools.clone())),
        "tools/call" => Some(call_handler(req)),
        _ => Some(Response::Error {
            code: -32601,
            message: format!("method not found: {}", req.method),
        }),
    })
}
