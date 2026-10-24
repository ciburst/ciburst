//! Per-pool providers (spec D35, M4 rule R8). The config's `provider` is the main member; each
//! key of `providers` adds a member serving the pools whose `runtime` names it. Run ids and step
//! ids of an added member start with `<runtime>:`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::config::Config;
use crate::log::Log;
use crate::types::{
    Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step, VmCapacity, VmReport,
};

pub struct Composite {
    config: Arc<Config>,
    main: Arc<dyn Provider>,
    extras: Vec<(String, Arc<dyn Provider>)>,
    log: Log,
}

impl Composite {
    pub fn new(
        config: Arc<Config>,
        main: Arc<dyn Provider>,
        extras: Vec<(String, Arc<dyn Provider>)>,
        log: Log,
    ) -> Composite {
        Composite {
            config,
            main,
            extras,
            log,
        }
    }

    fn extra(&self, runtime: &str) -> Option<&Arc<dyn Provider>> {
        self.extras
            .iter()
            .find(|(name, _)| name == runtime)
            .map(|(_, provider)| provider)
    }

    /// The member serving `tag` and whether it is an added one.
    fn for_pool(&self, tag: &str) -> (&Arc<dyn Provider>, bool) {
        let extra = self
            .config
            .pool_by_tag(tag)
            .and_then(|pool| self.config.extra_provider_of(pool))
            .and_then(|runtime| self.extra(runtime));
        match extra {
            Some(provider) => (provider, true),
            None => (&self.main, false),
        }
    }

    fn for_run(&self, run_id: &str) -> &Arc<dyn Provider> {
        self.extras
            .iter()
            .find(|(name, _)| owns(name, run_id))
            .map(|(_, provider)| provider)
            .unwrap_or(&self.main)
    }
}

/// Whether an id carries the prefix `<runtime>:`.
fn owns(runtime: &str, id: &str) -> bool {
    id.strip_prefix(runtime)
        .is_some_and(|rest| rest.starts_with(':'))
}

#[async_trait]
impl Provider for Composite {
    fn capabilities(&self) -> Capabilities {
        self.main.capabilities()
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let mut steps = self.main.plan(rotate).await?;
        for (_, provider) in &self.extras {
            steps.extend(provider.plan(rotate).await?);
        }
        Ok(steps)
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        let mine = |runtime: &str| -> Vec<Step> {
            steps
                .iter()
                .filter(|step| owns(runtime, &step.id))
                .cloned()
                .collect()
        };
        let main_steps: Vec<Step> = steps
            .iter()
            .filter(|step| !self.extras.iter().any(|(name, _)| owns(name, &step.id)))
            .cloned()
            .collect();
        let mut outputs = self.main.apply(&main_steps).await?;
        for (name, provider) in &self.extras {
            outputs.extend(provider.apply(&mine(name)).await?);
        }
        Ok(outputs)
    }

    async fn destroy(&self) -> Result<()> {
        for (_, provider) in &self.extras {
            provider.destroy().await?;
        }
        self.main.destroy().await
    }

    async fn start(&self, mut request: StartRequest) -> Result<String> {
        let (provider, extra) = self.for_pool(&request.pool.tag);
        if extra {
            for (key, value) in self.main.shared_env().await? {
                request.env.entry(key).or_insert(value);
            }
        }
        provider.start(request).await
    }

    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        match pool {
            Some(tag) => self.for_pool(tag).0.list(Some(tag)).await,
            None => {
                let mut runs = self.main.list(None).await?;
                for (_, provider) in &self.extras {
                    runs.extend(provider.list(None).await?);
                }
                Ok(runs)
            }
        }
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        self.for_run(run_id).stop(run_id).await
    }

    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        self.for_pool(pool).0.capacity(pool).await
    }

    async fn claim(&self, run_id: &str) -> Result<()> {
        self.for_run(run_id).claim(run_id).await
    }

    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        self.for_run(run_id).report(run_id, report, now).await
    }

    async fn shared_env(&self) -> Result<BTreeMap<String, String>> {
        self.main.shared_env().await
    }

    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
        let mut swept = Vec::new();
        let members = std::iter::once(("main", &self.main))
            .chain(self.extras.iter().map(|(name, p)| (name.as_str(), p)));
        for (name, provider) in members {
            match provider.sweep(now).await {
                Ok(runs) => swept.extend(runs),
                Err(error) => self.log.warn(
                    "sweep_failed",
                    &[("provider", name), ("error", &error.to_string())],
                ),
            }
        }
        Ok(swept)
    }
}
