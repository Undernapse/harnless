//! The core spine: the plugin that mounts the runtime services onto a
//! context.
//!
//! Per "no privileged core" (spec decision 03), the agent loop and session
//! log are *services*, not an entry point. This plugin provides them so the
//! loop is one registered service a consumer obtains via a typed accessor,
//! never by importing the loop implementation.
//!
//! Mounting the spine within its own fiber means unloading it unwinds the
//! loop and its registrations together.

use harnless_runtime::context::Context;
use harnless_runtime::events::EventRegistry;
use harnless_runtime::fiber::Fiber;
use harnless_runtime::plugin::Plugin;
use harnless_runtime::Result;

use harnless_seams::SessionId;

use crate::loop_::AgentLoop;
use crate::session::SessionLog;
use crate::tools::ToolRegistry;

/// The core spine plugin.
///
/// `apply` creates the [`SessionLog`], [`ToolRegistry`], and [`AgentLoop`]
/// and provides them as services on the mounted context. The spine must be
/// mounted inside a fiber (its owning plugin's fiber), so unloading the
/// plugin tears the whole spine chain down in order.
pub struct Spine {
    session_id: SessionId,
}

impl Spine {
    /// Create a spine for `session_id`.
    pub fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }
}

impl Plugin for Spine {
    fn name(&self) -> &str {
        "spine"
    }

    fn apply(&self, ctx: &Context) -> Result<()> {
        // The session log is the single source of truth.
        let log = SessionLog::new(self.session_id);
        ctx.provide(log)?;

        // The tool registry hosts the guarded pipeline's extension points.
        let events = EventRegistry::new();
        ctx.provide(events.clone())?;

        let fiber: std::sync::Arc<Fiber> =
            ctx.fiber().expect("spine must be mounted within a fiber");
        let registry = ToolRegistry::new(events.clone(), fiber.clone());
        ctx.provide(registry)?;

        // The loop drives the log; it gets the log back out of the context as
        // a shared handle.
        let log = ctx.get::<SessionLog>().expect("log just provided");
        let loop_ = AgentLoop::new(log, events, fiber);
        ctx.provide(loop_)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnless_runtime::context::Context;
    use harnless_runtime::plugin::Registry;
    use std::sync::Arc;

    #[test]
    fn mounting_spine_provides_core_services() {
        let registry = Registry::new();
        let ctx = Context::root();
        let fiber = registry
            .mount(&ctx, Arc::new(Spine::new(SessionId(1))))
            .unwrap();
        // The log, registry, loop, and event registry are all services.
        assert!(ctx.has::<SessionLog>());
        assert!(ctx.has::<ToolRegistry>());
        assert!(ctx.has::<AgentLoop>());
        assert!(ctx.has::<EventRegistry>());
        // Unmounting the spine tears the chain down.
        registry.unmount(&fiber);
        assert!(!ctx.has::<SessionLog>());
        assert!(!ctx.has::<ToolRegistry>());
        assert!(!ctx.has::<AgentLoop>());
    }

    #[test]
    fn spine_loop_drives_a_turn_into_the_shared_log() {
        let registry = Registry::new();
        let ctx = Context::root();
        let _f = registry
            .mount(&ctx, Arc::new(Spine::new(SessionId(2))))
            .unwrap();
        let log = ctx.get::<SessionLog>().unwrap();
        let loop_ = ctx.get::<AgentLoop>().unwrap();
        let text = "hi".to_string();
        let _ = loop_.run_turn(Box::new(move || {
            crate::loop_::DriverOutcome::Message(crate::events::MessageRecord {
                id: harnless_seams::MessageId(1),
                blocks: vec![crate::events::ContentBlock::Text { text: text.clone() }],
                provider: Some("replay".into()),
                model: Some("test".into()),
            })
        }));
        // The loop wrote into the same log the context holds as a service.
        assert_eq!(log.len(), 5);
    }
}
