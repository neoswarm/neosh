//! Driver and instance registry.
//!
//! [`DriverKind`](neosh_proto::DriverKind) is an open slug, so registering a driver is a runtime
//! operation. The host registers the built-ins at startup; a plugin registers its own through
//! `ApiCall::ProviderRegisterDriver`. Nothing here knows the set of vendors that exist.

use std::collections::BTreeMap;
use std::sync::Arc;

use neosh_proto::{DriverKind, InstanceConfig, InstanceId, ModelSelection};

use crate::Provider;

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    drivers: BTreeMap<DriverKind, Arc<dyn Provider>>,
    instances: BTreeMap<InstanceId, InstanceConfig>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("drivers", &self.drivers.keys().collect::<Vec<_>>())
            .field("instances", &self.instances.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_driver(&mut self, provider: Arc<dyn Provider>) {
        self.drivers.insert(provider.driver(), provider);
    }

    /// Add a configured instance.
    ///
    /// Instances referencing an unknown driver are kept rather than rejected: a plugin that
    /// registers its driver later should adopt instances the user already configured, instead of
    /// the user having to care about load order.
    pub fn add_instance(&mut self, cfg: InstanceConfig) {
        self.instances.insert(cfg.id.clone(), cfg);
    }

    pub fn remove_instance(&mut self, id: &InstanceId) {
        self.instances.remove(id);
    }

    /// Drop every configured instance, keeping the registered drivers.
    ///
    /// Used when reapplying configuration: rebuilding from a known base is the only way an
    /// instance the user *deleted* from  their config actually disappears, and the only way an
    /// instance that overrode a built-in reverts to the built-in when the override is removed.
    pub fn clear_instances(&mut self) {
        self.instances.clear();
    }

    /// Unregister a driver, e.g. because the plugin that provided it was unloaded.
    pub fn remove_driver(&mut self, kind: &DriverKind) {
        self.drivers.remove(kind);
    }

    pub fn instance(&self, id: &InstanceId) -> Option<&InstanceConfig> {
        self.instances.get(id)
    }

    pub fn instances(&self) -> impl Iterator<Item = &InstanceConfig> {
        self.instances.values()
    }

    /// Instances whose driver is actually available right now.
    pub fn usable_instances(&self) -> impl Iterator<Item = &InstanceConfig> {
        self.instances.values().filter(|c| self.drivers.contains_key(&c.driver))
    }

    pub fn driver(&self, kind: &DriverKind) -> Option<Arc<dyn Provider>> {
        self.drivers.get(kind).cloned()
    }

    pub fn drivers(&self) -> impl Iterator<Item = &DriverKind> {
        self.drivers.keys()
    }

    /// Resolve a selection to the driver and instance that serve it.
    pub fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Option<(Arc<dyn Provider>, &InstanceConfig)> {
        let inst = self.instances.get(&selection.instance)?;
        let drv = self.drivers.get(&inst.driver)?.clone();
        Some((drv, inst))
    }

    /// Pick a sensible starting selection.
    ///
    /// Preference order is described on [`default_selection`](Self::default_selection). The catalog arranges so that
    /// a zero-configuration install lands on a driver that needs no API key.
    /// Instances whose credentials are present, so a first run picks something that will work.
    ///
    /// Asks the credential store rather than the [`AuthRef`](neosh_proto::AuthRef) alone: a key you
    /// typed in last week and a key in `$ANTHROPIC_API_KEY` are equally present, and an instance
    /// that only looked unusable because the environment was empty should stop looking unusable the
    /// moment you sign in.
    pub fn ready_instances(&self) -> impl Iterator<Item = &InstanceConfig> {
        let store = crate::credentials::credentials();
        self.usable_instances().filter(move |c| store.source(c).is_usable())
    }

    /// Every instance with the state of its credential, for a settings list.
    pub fn credentials(&self) -> Vec<neosh_proto::CredentialInfo> {
        crate::credentials::credentials()
            .survey(self.instances(), |c| self.drivers.contains_key(&c.driver))
    }

    /// What to use when nothing has been chosen.
    ///
    /// Credentialed instances come first. `instances` is a `BTreeMap`, so "first" is alphabetical
    /// rather than the catalog's order — which used to mean a fresh install with no API key
    /// defaulted to `anthropic` and failed on the first message, while `claude-cli`, which needs no
    /// key and was listed first precisely for that reason, sat unused.
    ///
    /// Falls back to any usable instance so that `--list-models` and an explicit `--model` still
    /// behave when nothing is configured; the status line then shows what was picked.
    pub fn default_selection(&self) -> Option<ModelSelection> {
        let inst = self.ready_instances().next().or_else(|| self.usable_instances().next())?;
        let model = inst.models.first()?;
        Some(ModelSelection {
            instance: inst.id.clone(),
            model: model.id.clone(),
            options: crate::catalog::default_options(&model.capabilities),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{AuthRef, ModelId, ModelInfo};

    #[test]
    fn a_first_run_defaults_to_something_that_will_actually_work() {
        // Regression: `instances` is a BTreeMap, so "first" is alphabetical. With no API key set
        // that made `anthropic` the default over `claude-cli`, and the first message failed —
        // even though `claude-cli` needs no key and the catalog lists it first for that reason.
        let mut r = ProviderRegistry::new();
        r.register_driver(std::sync::Arc::new(crate::drivers::MockProvider::new(vec![])));

        let model = |id: &str| ModelInfo {
            id: ModelId::from(id),
            display_name: id.into(),
            context_window: None,
            max_output_tokens: None,
            capabilities: Default::default(),
            pricing: None,
        };
        let inst = |id: &str, auth: AuthRef| InstanceConfig {
            id: InstanceId::from(id),
            driver: DriverKind::from("mock"),
            display_name: id.into(),
            base_url: None,
            auth,
            models: vec![model("m")],
            extra_headers: vec![],
        };

        // "aaa" sorts first but has no credential; "zzz" needs none.
        r.add_instance(inst("aaa-needs-key", AuthRef::Env {
            var: "NEOSH_TEST_DEFINITELY_UNSET_KEY".into(),
        }));
        r.add_instance(inst("zzz-no-key-needed", AuthRef::Inherited));

        let chosen = r.default_selection().expect("something is selectable");
        assert_eq!(
            chosen.instance,
            InstanceId::from("zzz-no-key-needed"),
            "picked an instance whose credential is missing"
        );
    }

    #[test]
    fn a_default_is_still_offered_when_nothing_has_credentials() {
        // Otherwise `--list-models` and an explicit `--model` would have nothing to fall back to.
        let mut r = ProviderRegistry::new();
        r.register_driver(std::sync::Arc::new(crate::drivers::MockProvider::new(vec![])));
        r.add_instance(InstanceConfig {
            id: InstanceId::from("only"),
            driver: DriverKind::from("mock"),
            display_name: "only".into(),
            base_url: None,
            auth: AuthRef::Env { var: "NEOSH_TEST_DEFINITELY_UNSET_KEY".into() },
            models: vec![ModelInfo {
                id: ModelId::from("m"),
                display_name: "m".into(),
                context_window: None,
                max_output_tokens: None,
                capabilities: Default::default(),
                pricing: None,
            }],
            extra_headers: vec![],
        });
        assert!(r.default_selection().is_some());
    }

}
