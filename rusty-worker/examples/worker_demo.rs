//! Demo worker: two handlers behind the remote-node protocol.
//!
//! - `greeter` — a plain node: reads `state.name`, writes `greeting`.
//! - `approval_gate` — a HITL node: interrupts with an approval question
//!   until the run is resumed; on resume it writes `approved`.
//!
//! Run it:
//!
//! ```sh
//! cargo run --example worker_demo
//! ```
//!
//! Then point a `RemoteNode` at it from any graph:
//!
//! ```ignore
//! use rusty_agent_runtime::remote::RemoteNode;
//!
//! builder.add_node("greet", RemoteNode::new("greeter", "http://127.0.0.1:8200"));
//! builder.add_node("gate", RemoteNode::new("approval_gate", "http://127.0.0.1:8200"));
//! ```
//!
//! Or probe it by hand (`probe_body()` builds this JSON with the current
//! protocol version):
//!
//! ```sh
//! curl http://127.0.0.1:8200/ok
//! curl -X POST http://127.0.0.1:8200/execute \
//!   -H 'content-type: application/json' \
//!   -d '{"protocol_version":1,"node":"greeter","state":{"name":"rustacean"},
//!        "config":{"thread_id":"t-1","step":0,"resume":null,"extra":{}}}'
//! ```

use rusty_agent_runtime::prelude::*;
use rusty_worker::{serve, WorkerRegistry};
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,rusty_worker=info".into()),
        )
        .init();

    let mut registry = WorkerRegistry::new();

    // A normal handler: any async closure `Fn(NodeContext) -> Result<NodeOutput>`
    // works, exactly like `GraphBuilder::add_node`.
    registry.register("greeter", |ctx: NodeContext| async move {
        let name = ctx
            .state()
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("world")
            .to_string();
        Ok(NodeOutput::update(
            "greeting",
            json!(format!("hello, {name}! (computed on remote worker)")),
        ))
    });

    // A HITL handler: interrupts until the caller resumes the run. The
    // interrupt payload travels back across the wire and suspends the whole
    // graph; the resume value crosses the wire again in `NodeTask::config`.
    registry.register("approval_gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(value) => Ok(NodeOutput::update("approved", value.clone())),
            None => Err(ctx.interrupt(json!({
                "question": "approve deployment?",
                "node": "approval_gate",
                "thread_id": ctx.thread_id(),
            }))),
        }
    });

    let addr = "127.0.0.1:8200";
    println!("rusty worker demo");
    println!(
        "  serving handlers: {:?}",
        registry.names().collect::<Vec<_>>()
    );
    println!("  listening on      http://{addr}");
    println!();
    println!("Point a RemoteNode at it from any graph:");
    println!();
    println!("  builder.add_node(\"greet\", RemoteNode::new(\"greeter\", \"http://{addr}\"));");
    println!(
        "  builder.add_node(\"gate\",  RemoteNode::new(\"approval_gate\", \"http://{addr}\"));"
    );
    println!();
    println!("Or probe it by hand:");
    println!("  curl http://{addr}/ok");
    // Built from `probe_body()` so the printed snippet always carries the
    // current PROTOCOL_VERSION and task shape instead of a stale literal.
    let mut probe = rusty_worker::probe_body();
    probe["node"] = json!("greeter");
    probe["state"] = json!({"name": "rustacean"});
    println!("  curl -X POST http://{addr}/execute -H 'content-type: application/json' \\");
    println!(
        "    -d '{}'",
        serde_json::to_string(&probe).expect("probe body serializes")
    );

    serve(registry, addr).await
}
