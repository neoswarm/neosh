//! Pre/post hooks, and the veto path.
//!
//! # Blocking versus observing
//!
//! A hook registered with `blocking: false` is a pure observer: it is fired and forgotten, and its
//! return value is ignored. That is deliberate — an audit or telemetry plugin must not be able to
//! wedge the agent loop, and making that structural is better than asking plugin authors to be
//! careful.
//!
//! A blocking hook is awaited, and can [`HookOutcome::Modify`] the payload or
//! [`HookOutcome::Veto`] the action outright.
//!
//! # Timeouts fail closed
//!
//! A blocking hook that does not answer within its timeout is treated as a **veto**. The
//! alternative — proceeding when the policy plugin is wedged — means a permission layer that stops
//! working precisely when something is wrong with it. Observers cannot cause this, because they
//! are never awaited.

use std::collections::BTreeMap;
use std::time::Duration;

use neosh_proto::{HookName, HookOutcome, HookPayload, PluginId};

pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReg {
    pub plugin: PluginId,
    pub blocking: bool,
    pub timeout: Duration,
}

#[derive(Debug, Default, Clone)]
pub struct HookRegistry {
    regs: BTreeMap<HookName, Vec<HookReg>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        hook: HookName,
        plugin: &PluginId,
        blocking: bool,
        timeout_ms: Option<u64>,
    ) {
        let entry = self.regs.entry(hook).or_default();
        entry.retain(|r| &r.plugin != plugin);
        entry.push(HookReg {
            plugin: plugin.clone(),
            blocking,
            timeout: timeout_ms.map(Duration::from_millis).unwrap_or(DEFAULT_HOOK_TIMEOUT),
        });
    }

    pub fn unregister(&mut self, hook: HookName, plugin: &PluginId) {
        if let Some(v) = self.regs.get_mut(&hook) {
            v.retain(|r| &r.plugin != plugin);
        }
    }

    pub fn remove_plugin(&mut self, plugin: &PluginId) {
        for v in self.regs.values_mut() {
            v.retain(|r| &r.plugin != plugin);
        }
    }

    pub fn blocking(&self, hook: HookName) -> impl Iterator<Item = &HookReg> {
        self.regs.get(&hook).into_iter().flatten().filter(|r| r.blocking)
    }

    pub fn observers(&self, hook: HookName) -> impl Iterator<Item = &HookReg> {
        self.regs.get(&hook).into_iter().flatten().filter(|r| !r.blocking)
    }

    pub fn has_blocking(&self, hook: HookName) -> bool {
        self.blocking(hook).next().is_some()
    }
}

/// Run every blocking hook for `hook` in registration order.
///
/// Short-circuits on the first veto: once an action is refused there is no reason to keep asking,
/// and later hooks must not be able to overturn a refusal.
pub async fn run_blocking(
    regs: Vec<HookReg>,
    bridge: &dyn crate::PluginBridge,
    hook: HookName,
    mut payload: HookPayload,
) -> (HookOutcome, HookPayload) {
    // Takes an owned list rather than a `&HookRegistry` on purpose: borrowing the registry would
    // hold its lock across every hook call, so a plugin registering a hook from inside a hook would
    // deadlock — and the resulting future would not be `Send`, so the turn could not be spawned.
    for r in regs {
        let outcome = match tokio::time::timeout(
            r.timeout,
            bridge.call_hook(&r.plugin, hook, payload.clone()),
        )
        .await
        {
            Ok(o) => o,
            Err(_) => {
                tracing::warn!(
                    plugin = %r.plugin, ?hook, timeout_ms = r.timeout.as_millis(),
                    "blocking hook timed out; treating as a veto"
                );
                HookOutcome::Veto {
                    reason: format!(
                        "{} did not answer the {hook:?} hook within {:?}",
                        r.plugin, r.timeout
                    ),
                }
            }
        };

        match outcome {
            HookOutcome::Continue => {}
            HookOutcome::Veto { reason } => return (HookOutcome::Veto { reason }, payload),
            HookOutcome::Modify { payload: raw } => {
                match serde_json::from_value::<HookPayload>(raw.clone()) {
                    Ok(next) => payload = next,
                    Err(e) => {
                        // A hook that returns something unusable must not silently no-op: that
                        // would look like the modification was applied.
                        return (
                            HookOutcome::Veto {
                                reason: format!(
                                    "{} returned an unusable {hook:?} payload: {e}",
                                    r.plugin
                                ),
                            },
                            payload,
                        );
                    }
                }
            }
        }
    }
    (HookOutcome::Continue, payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{PluginEvent, ToolCall, ToolCallId, TurnId};
    use std::sync::{Arc, Mutex};

    fn payload() -> HookPayload {
        HookPayload::ToolPre {
            call: ToolCall {
                id: ToolCallId("t".into()),
                turn: TurnId::from("turn"),
                name: "read_file".into(),
                input: serde_json::json!({"path": "a.txt"}),
            },
        }
    }

    #[derive(Default)]
    struct FakeBridge {
        outcome: Mutex<Option<HookOutcome>>,
        /// Simulates a wedged plugin.
        hang: bool,
        pub called: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::PluginBridge for FakeBridge {
        async fn run_tool(
            &self,
            _p: &PluginId,
            _n: &str,
            _i: serde_json::Value,
        ) -> neosh_proto::ToolResult {
            neosh_proto::ToolResult::ok("")
        }
        async fn call_hook(
            &self,
            p: &PluginId,
            _h: HookName,
            _pl: HookPayload,
        ) -> HookOutcome {
            self.called.lock().unwrap().push(p.0.clone());
            if self.hang {
                // Longer than any timeout under test.
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            self.outcome.lock().unwrap().clone().unwrap_or(HookOutcome::Continue)
        }
        fn broadcast(&self, _e: PluginEvent) {}
        fn notify(&self, _p: &PluginId, _e: PluginEvent) {}
    }

    fn reg_with(blocking: bool, timeout_ms: Option<u64>) -> HookRegistry {
        let mut r = HookRegistry::new();
        r.register(HookName::ToolPre, &PluginId::from("p"), blocking, timeout_ms);
        r
    }

    #[tokio::test]
    async fn a_blocking_hook_can_veto() {
        let b = FakeBridge {
            outcome: Mutex::new(Some(HookOutcome::Veto { reason: "nope".into() })),
            ..Default::default()
        };
        let (o, _) = run_blocking(reg_with(true, None).blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        assert_eq!(o, HookOutcome::Veto { reason: "nope".into() });
    }

    #[tokio::test]
    async fn an_observer_cannot_veto() {
        let b = FakeBridge {
            outcome: Mutex::new(Some(HookOutcome::Veto { reason: "nope".into() })),
            ..Default::default()
        };
        // Registered non-blocking, so run_blocking never even calls it.
        let (o, _) = run_blocking(reg_with(false, None).blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        assert_eq!(o, HookOutcome::Continue);
        assert!(b.called.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocking_hook_that_hangs_is_treated_as_a_veto() {
        let b = FakeBridge { hang: true, ..Default::default() };
        let (o, _) =
            run_blocking(reg_with(true, Some(50)).blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        match o {
            HookOutcome::Veto { reason } => assert!(reason.contains("did not answer")),
            other => panic!("a wedged permission plugin must fail closed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_veto_short_circuits_later_hooks() {
        let mut reg = HookRegistry::new();
        reg.register(HookName::ToolPre, &PluginId::from("first"), true, None);
        reg.register(HookName::ToolPre, &PluginId::from("second"), true, None);
        let b = FakeBridge {
            outcome: Mutex::new(Some(HookOutcome::Veto { reason: "no".into() })),
            ..Default::default()
        };
        let (_, _) = run_blocking(reg.blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        assert_eq!(b.called.lock().unwrap().as_slice(), ["first"]);
    }

    #[tokio::test]
    async fn a_modify_that_cannot_be_decoded_becomes_a_veto() {
        let b = FakeBridge {
            outcome: Mutex::new(Some(HookOutcome::Modify {
                payload: serde_json::json!({"nonsense": true}),
            })),
            ..Default::default()
        };
        let (o, _) = run_blocking(reg_with(true, None).blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        assert!(
            matches!(o, HookOutcome::Veto { .. }),
            "a silently-ignored modification is worse than a refusal"
        );
    }

    #[tokio::test]
    async fn a_modification_is_visible_to_the_next_hook_and_the_caller() {
        let mut reg = HookRegistry::new();
        reg.register(HookName::ToolPre, &PluginId::from("p"), true, None);
        let rewritten = HookPayload::ToolPre {
            call: ToolCall {
                id: ToolCallId("t".into()),
                turn: TurnId::from("turn"),
                name: "read_file".into(),
                input: serde_json::json!({"path": "safe.txt"}),
            },
        };
        let b = FakeBridge {
            outcome: Mutex::new(Some(HookOutcome::Modify {
                payload: serde_json::to_value(&rewritten).unwrap(),
            })),
            ..Default::default()
        };
        let (o, out) = run_blocking(reg.blocking(HookName::ToolPre).cloned().collect(), &b, HookName::ToolPre, payload()).await;
        assert_eq!(o, HookOutcome::Continue);
        assert_eq!(out, rewritten);
    }

    #[test]
    fn re_registering_replaces_rather_than_duplicates() {
        let mut r = HookRegistry::new();
        r.register(HookName::ToolPre, &PluginId::from("p"), false, None);
        r.register(HookName::ToolPre, &PluginId::from("p"), true, Some(10));
        assert_eq!(r.blocking(HookName::ToolPre).count(), 1);
        assert_eq!(r.observers(HookName::ToolPre).count(), 0);
    }

    #[test]
    fn unloading_a_plugin_drops_its_hooks() {
        let mut r = HookRegistry::new();
        r.register(HookName::ToolPre, &PluginId::from("p"), true, None);
        r.remove_plugin(&PluginId::from("p"));
        assert!(!r.has_blocking(HookName::ToolPre));
    }
}
