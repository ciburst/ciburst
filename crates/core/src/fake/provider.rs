use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};

use crate::config::{CacheType, Config, Pool};
use crate::provision::keys;
use crate::store::Store;
use crate::types::{
    Action, Billing, Capabilities, KeyPair, Outputs, Provider, Rotation, Run, RunState,
    StartRequest, Step, VmCapacity, VmReport, VmState,
};
use crate::vm;
use crate::{Error, Result};

/// A pool with this `runtime` runs on fake VMs: several slots, reports, claims and the sweep.
pub const VM_RUNTIME: &str = "instances";

/// Unix seconds; injectable so that simulations control time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Definition {
    image: String,
    vcpu: u32,
    memory_mib: u32,
    storage_mib: u32,
    timeout_s: u32,
    wait_timeout_s: u32,
}

impl From<&Pool> for Definition {
    fn from(pool: &Pool) -> Definition {
        Definition {
            image: pool.image.clone(),
            vcpu: pool.vcpu,
            memory_mib: pool.memory_mib,
            storage_mib: pool.storage_mib,
            timeout_s: pool.timeout_s,
            wait_timeout_s: pool.wait_timeout_s,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
    name: String,
    expire_days: u32,
}

/// What the real provider keeps in a server's tags (spec 6.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Vm {
    run_id: String,
    pool: String,
    state: VmState,
    free: u32,
    seen: Option<u64>,
    drain: Option<u64>,
    created_at: u64,
}

impl Vm {
    fn capacity(&self) -> VmCapacity {
        VmCapacity {
            run_id: self.run_id.clone(),
            state: self.state,
            free: self.free,
            seen: self.seen,
            created_at: self.created_at,
        }
    }

    /// Rule 10 of the M3 milestone, with the limits of `crate::vm`.
    fn swept(&self, now: u64) -> bool {
        let since = |t: u64| now.saturating_sub(t);
        if since(self.created_at) > vm::SWEEP_MAX_AGE_S {
            return true;
        }
        if self
            .seen
            .is_some_and(|seen| since(seen) <= vm::SWEEP_SILENT_S)
        {
            return false;
        }
        let starting =
            self.state == VmState::Starting && since(self.created_at) > vm::SWEEP_STARTING_S;
        let draining = self.state == VmState::Draining
            && self.drain.is_some_and(|d| since(d) > vm::SWEEP_DRAINING_S);
        let silent = since(self.seen.unwrap_or(self.created_at)) > vm::SWEEP_SILENT_S;
        starting || draining || silent
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    project: Option<String>,
    keys: BTreeMap<String, KeyPair>,
    bucket: Option<Bucket>,
    definitions: BTreeMap<String, Definition>,
    runs: Vec<Run>,
    #[serde(default)]
    vms: Vec<Vm>,
}

pub struct FakeProvider {
    config: Arc<Config>,
    store: Arc<dyn Store>,
    clock: Clock,
}

impl FakeProvider {
    pub fn new(config: Arc<Config>, store: Arc<dyn Store>) -> FakeProvider {
        FakeProvider {
            config,
            store,
            clock: Arc::new(now_seconds),
        }
    }

    /// Replaces the system clock, which stamps `created_at` of new runs.
    pub fn with_clock(mut self, clock: Clock) -> FakeProvider {
        self.clock = clock;
        self
    }

    fn is_vm_pool(&self, tag: &str) -> bool {
        self.config
            .pool_by_tag(tag)
            .is_some_and(|pool| pool.runtime.as_deref() == Some(VM_RUNTIME))
    }

    async fn state(&self) -> Result<State> {
        match self.store.load().await? {
            Some(value) => Ok(serde_json::from_value(value)?),
            None => Ok(State::default()),
        }
    }

    async fn save(&self, state: &State) -> Result<()> {
        self.store.save(Some(serde_json::to_value(state)?)).await
    }

    fn definition_name(&self, tag: &str) -> String {
        format!("{}-{tag}", self.config.name)
    }

    fn key_step(&self, id: &str, label: &str, present: bool, rotate: bool) -> Step {
        let action = if !present {
            Action::Create
        } else if rotate {
            Action::Update
        } else {
            Action::Noop
        };
        let step = Step::new(id, action, format!("api key {}-{label}", self.config.name));
        if action == Action::Update {
            step.with_detail("rotate")
        } else {
            step
        }
    }

    /// Test helper outside the contract.
    pub async fn set_run_state(&self, run_id: &str, run_state: RunState) -> Result<()> {
        let mut state = self.state().await?;
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.run_id == run_id)
            .ok_or_else(|| Error::provider(format!("unknown run {run_id}")))?;
        run.state = run_state;
        self.save(&state).await
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            name: "fake".into(),
            docker_daemon: false,
            max_vcpu: 6,
            max_memory_mib: 16384,
            max_storage_mib: 10240,
            max_duration_s: 86400,
            archs: vec!["amd64".into()],
            billing: Billing::PerSecond,
        }
    }

    async fn plan(&self, rotate: &[Rotation]) -> Result<Vec<Step>> {
        let state = self.state().await?;
        let name = &self.config.name;
        let mut steps = vec![Step::new(
            "project",
            if state.project.is_some() {
                Action::Noop
            } else {
                Action::Create
            },
            format!("project {name}"),
        )];
        steps.push(self.key_step(
            "key:dispatch",
            "dispatch",
            state.keys.contains_key("dispatch"),
            rotate.contains(&Rotation::Key),
        ));
        if self.config.cache.kind == CacheType::S3 {
            steps.push(self.key_step(
                "key:cache",
                "cache",
                state.keys.contains_key("cache"),
                rotate.contains(&Rotation::Cache),
            ));
            let action = match &state.bucket {
                None => Action::Create,
                Some(bucket) if bucket.expire_days == self.config.cache.expire_days => Action::Noop,
                Some(_) => Action::Update,
            };
            steps.push(Step::new(
                "bucket",
                action,
                format!("cache bucket {name}-cache"),
            ));
        } else {
            if state.keys.contains_key("cache") {
                steps.push(Step::new(
                    "key:cache",
                    Action::Delete,
                    format!("api key {name}-cache"),
                ));
            }
            if state.bucket.is_some() {
                steps.push(Step::new(
                    "bucket",
                    Action::Delete,
                    format!("cache bucket {name}-cache"),
                ));
            }
        }
        for pool in &self.config.pools {
            let action = match state.definitions.get(&pool.tag) {
                None => Action::Create,
                Some(current) if *current == Definition::from(pool) => Action::Noop,
                Some(_) => Action::Update,
            };
            steps.push(Step::new(
                format!("definition:{}", pool.tag),
                action,
                format!("job definition {}", self.definition_name(&pool.tag)),
            ));
        }
        for tag in state.definitions.keys() {
            if self.config.pool_by_tag(tag).is_none() {
                steps.push(Step::new(
                    format!("definition:{tag}"),
                    Action::Delete,
                    format!("job definition {}", self.definition_name(tag)),
                ));
            }
        }
        Ok(steps)
    }

    async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
        let mut state = self.state().await?;
        let mut outputs = Outputs::new();
        for step in steps {
            if step.is_noop() {
                continue;
            }
            match step.id.as_str() {
                "project" => state.project = Some(random_id(32)),
                "key:dispatch" => {
                    let key = fake_key();
                    outputs.insert("CIBURST_DISPATCH_ACCESS_KEY".into(), key.access_key.clone());
                    outputs.insert("CIBURST_DISPATCH_SECRET_KEY".into(), key.secret_key.clone());
                    state.keys.insert("dispatch".into(), key);
                }
                "key:cache" => {
                    if step.action == Action::Delete {
                        state.keys.remove("cache");
                    } else {
                        let key = fake_key();
                        outputs.insert("CIBURST_CACHE_ACCESS_KEY".into(), key.access_key.clone());
                        outputs.insert("CIBURST_CACHE_SECRET_KEY".into(), key.secret_key.clone());
                        state.keys.insert("cache".into(), key);
                    }
                }
                "bucket" => {
                    state.bucket = if step.action == Action::Delete {
                        None
                    } else {
                        Some(Bucket {
                            name: format!("{}-cache", self.config.name),
                            expire_days: self.config.cache.expire_days,
                        })
                    };
                }
                id => {
                    let tag = id
                        .strip_prefix("definition:")
                        .ok_or_else(|| Error::provider(format!("unknown step {id}")))?;
                    if step.action == Action::Delete {
                        state.definitions.remove(tag);
                    } else {
                        let pool = self
                            .config
                            .pool_by_tag(tag)
                            .ok_or_else(|| Error::provider(format!("unknown pool {tag}")))?;
                        state
                            .definitions
                            .insert(tag.to_string(), Definition::from(pool));
                    }
                }
            }
        }
        self.save(&state).await?;
        Ok(outputs)
    }

    async fn destroy(&self) -> Result<()> {
        self.save(&State::default()).await
    }

    async fn start(&self, request: StartRequest) -> Result<String> {
        let mut state = self.state().await?;
        if !state.definitions.contains_key(&request.pool.tag) {
            return Err(Error::provider(format!(
                "pool {} is not provisioned",
                request.pool.tag
            )));
        }
        if request.env.get("RUNNER_TOKEN").is_none_or(|t| t.is_empty()) {
            return Err(Error::provider("RUNNER_TOKEN missing"));
        }
        let vm = self.is_vm_pool(&request.pool.tag);
        if vm
            && request
                .env
                .get(keys::VM_SECRET)
                .is_none_or(|s| s.is_empty())
        {
            return Err(Error::provider(format!("{} missing", keys::VM_SECRET)));
        }
        let created_at = (self.clock)();
        let run = Run {
            run_id: format!("fake-{}", random_id(12)),
            pool: request.pool.tag.clone(),
            state: RunState::Queued,
            created_at: created_at.to_string(),
            labels: request.labels,
        };
        let run_id = run.run_id.clone();
        if vm {
            state.vms.push(Vm {
                run_id: run_id.clone(),
                pool: run.pool.clone(),
                state: VmState::Starting,
                free: request.pool.slots,
                seen: None,
                drain: None,
                created_at,
            });
        }
        state.runs.push(run);
        self.save(&state).await?;
        Ok(run_id)
    }

    async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
        Ok(self
            .state()
            .await?
            .runs
            .into_iter()
            .filter(|run| pool.is_none_or(|p| run.pool == p))
            .collect())
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        let mut state = self.state().await?;
        stop_run(&mut state, run_id)?;
        self.save(&state).await
    }

    async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
        if !self.is_vm_pool(pool) {
            return Ok(None);
        }
        Ok(Some(
            self.state()
                .await?
                .vms
                .iter()
                .filter(|vm| vm.pool == pool)
                .map(Vm::capacity)
                .collect(),
        ))
    }

    async fn claim(&self, run_id: &str) -> Result<()> {
        let mut state = self.state().await?;
        let vm = vm_mut(&mut state, run_id)?;
        vm.free = vm.free.saturating_sub(1);
        self.save(&state).await
    }

    async fn report(&self, run_id: &str, report: &VmReport, now: u64) -> Result<()> {
        let mut state = self.state().await?;
        let vm = vm_mut(&mut state, run_id)?;
        vm.state = report.state;
        vm.free = report.free;
        vm.seen = Some(now);
        vm.drain = match report.state {
            VmState::Draining => vm.drain.or(Some(now)),
            _ => None,
        };
        let run_state = match report.state {
            VmState::Starting => RunState::Queued,
            _ => RunState::Running,
        };
        if let Some(run) = state.runs.iter_mut().find(|r| r.run_id == run_id) {
            run.state = run_state;
        }
        self.save(&state).await
    }

    async fn sweep(&self, now: u64) -> Result<Vec<String>> {
        let mut state = self.state().await?;
        let swept: Vec<String> = state
            .vms
            .iter()
            .filter(|vm| vm.swept(now))
            .map(|vm| vm.run_id.clone())
            .collect();
        for run_id in &swept {
            stop_run(&mut state, run_id)?;
        }
        self.save(&state).await?;
        Ok(swept)
    }
}

/// Ends a run; a VM run also leaves the capacity list, as a terminated server does.
fn stop_run(state: &mut State, run_id: &str) -> Result<()> {
    let run = state
        .runs
        .iter_mut()
        .find(|r| r.run_id == run_id)
        .ok_or_else(|| Error::provider(format!("unknown run {run_id}")))?;
    run.state = RunState::Done;
    state.vms.retain(|vm| vm.run_id != run_id);
    Ok(())
}

fn vm_mut<'a>(state: &'a mut State, run_id: &str) -> Result<&'a mut Vm> {
    state
        .vms
        .iter_mut()
        .find(|vm| vm.run_id == run_id)
        .ok_or_else(|| Error::provider(format!("unknown VM {run_id}")))
}

fn random_id(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(|c| char::from(c).to_ascii_lowercase())
        .collect()
}

fn fake_key() -> KeyPair {
    KeyPair {
        access_key: format!("SCWFAKE{}", random_id(12).to_ascii_uppercase()),
        secret_key: random_id(36),
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::store::MemoryStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    const T0: u64 = 1_800_000_000;

    fn config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "defaultPool": "vm",
                  "pools": [
                    { "tag": "vm", "image": "img/node", "runtime": "instances", "slots": 2 },
                    { "tag": "jobs", "image": "img/node", "runtime": "jobs", "stages": ["build"] }
                  ]
                }"#,
            )
            .unwrap(),
        )
    }

    async fn setup() -> (FakeProvider, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(T0));
        let clock = now.clone();
        let provider = FakeProvider::new(config(), Arc::new(MemoryStore::new()))
            .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)));
        provider
            .apply(&provider.plan(&[]).await.unwrap())
            .await
            .unwrap();
        (provider, now)
    }

    fn request(tag: &str, vm_secret: bool) -> StartRequest {
        let config = config();
        let mut env = BTreeMap::from([("RUNNER_TOKEN".to_string(), "glrt-x".to_string())]);
        if vm_secret {
            env.insert(keys::VM_SECRET.to_string(), "vm-secret".to_string());
        }
        StartRequest {
            pool: config.pool_by_tag(tag).unwrap().clone(),
            env,
            labels: BTreeMap::new(),
        }
    }

    fn report(state: VmState, free: u32) -> VmReport {
        VmReport {
            state,
            free,
            slots: 2,
        }
    }

    async fn only(provider: &FakeProvider) -> VmCapacity {
        let mut list = provider.capacity("vm").await.unwrap().unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        list.remove(0)
    }

    #[tokio::test]
    async fn one_job_pools_have_no_capacity() {
        let (provider, _) = setup().await;
        assert_eq!(provider.capacity("jobs").await.unwrap(), None);
        assert_eq!(provider.capacity("unknown").await.unwrap(), None);
        let run = provider.start(request("jobs", false)).await.unwrap();
        assert!(provider.claim(&run).await.is_err());
        assert!(
            provider
                .report(&run, &report(VmState::Idle, 1), T0)
                .await
                .is_err()
        );
        assert!(provider.sweep(T0 + 10 * 86_400).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_vm_starts_reports_and_gets_claimed() {
        let (provider, now) = setup().await;
        let error = provider.start(request("vm", false)).await.unwrap_err();
        assert!(error.to_string().contains("CIBURST_VM_SECRET"), "{error}");
        assert_eq!(provider.capacity("vm").await.unwrap(), Some(Vec::new()));

        let run = provider.start(request("vm", true)).await.unwrap();
        let vm = only(&provider).await;
        assert_eq!(
            vm,
            VmCapacity {
                run_id: run.clone(),
                state: VmState::Starting,
                free: 2,
                seen: None,
                created_at: T0,
            }
        );
        assert_eq!(
            provider.list(Some("vm")).await.unwrap()[0].state,
            RunState::Queued
        );

        now.store(T0 + 90, Ordering::SeqCst);
        provider
            .report(&run, &report(VmState::Idle, 2), T0 + 90)
            .await
            .unwrap();
        let vm = only(&provider).await;
        assert_eq!(
            (vm.state, vm.free, vm.seen),
            (VmState::Idle, 2, Some(T0 + 90))
        );
        assert_eq!(
            provider.list(Some("vm")).await.unwrap()[0].state,
            RunState::Running
        );

        for expected in [1, 0, 0] {
            provider.claim(&run).await.unwrap();
            assert_eq!(only(&provider).await.free, expected);
        }
        provider
            .report(&run, &report(VmState::Busy, 1), T0 + 95)
            .await
            .unwrap();
        assert_eq!(only(&provider).await.free, 1);
    }

    #[tokio::test]
    async fn draining_keeps_its_start_time_until_the_vm_is_busy_again() {
        let (provider, _) = setup().await;
        let run = provider.start(request("vm", true)).await.unwrap();
        provider
            .report(&run, &report(VmState::Draining, 2), T0 + 100)
            .await
            .unwrap();
        provider
            .report(&run, &report(VmState::Draining, 2), T0 + 120)
            .await
            .unwrap();
        assert_eq!(provider.state().await.unwrap().vms[0].drain, Some(T0 + 100));
        provider
            .report(&run, &report(VmState::Busy, 1), T0 + 125)
            .await
            .unwrap();
        assert_eq!(provider.state().await.unwrap().vms[0].drain, None);
    }

    #[tokio::test]
    async fn sweep_follows_the_limits() {
        let (provider, now) = setup().await;
        let start = |at: u64| {
            now.store(at, Ordering::SeqCst);
            provider.start(request("vm", true))
        };
        let sweep_at = T0 + vm::SWEEP_SILENT_S + 100;
        let at = |ago: u64| sweep_at - ago;
        let starting_old = start(at(vm::SWEEP_STARTING_S + 1)).await.unwrap();
        let starting_young = start(at(vm::SWEEP_STARTING_S)).await.unwrap();
        let silent = start(T0).await.unwrap();
        let quiet_enough = start(T0).await.unwrap();
        let draining_silent = start(T0).await.unwrap();
        let busy_ancient = start(at(vm::SWEEP_MAX_AGE_S + 1)).await.unwrap();
        for (run, state, seen) in [
            (&silent, VmState::Idle, at(vm::SWEEP_SILENT_S + 1)),
            (&quiet_enough, VmState::Idle, at(vm::SWEEP_SILENT_S)),
            (
                &draining_silent,
                VmState::Draining,
                at(vm::SWEEP_SILENT_S + 1),
            ),
            (&busy_ancient, VmState::Busy, at(10)),
        ] {
            provider.report(run, &report(state, 0), seen).await.unwrap();
        }

        let mut swept = provider.sweep(sweep_at).await.unwrap();
        swept.sort();
        let mut expected = vec![
            starting_old.clone(),
            silent.clone(),
            draining_silent.clone(),
            busy_ancient.clone(),
        ];
        expected.sort();
        assert_eq!(swept, expected);
        let mut left: Vec<String> = provider
            .capacity("vm")
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|v| v.run_id)
            .collect();
        left.sort();
        let mut kept = vec![starting_young, quiet_enough];
        kept.sort();
        assert_eq!(left, kept);
        let done = provider.list(Some("vm")).await.unwrap();
        assert!(
            done.iter()
                .filter(|r| expected.contains(&r.run_id))
                .all(|r| r.state == RunState::Done)
        );
    }

    #[tokio::test]
    async fn a_draining_vm_that_still_reports_is_kept() {
        let (provider, _) = setup().await;
        let run = provider.start(request("vm", true)).await.unwrap();
        provider
            .report(&run, &report(VmState::Draining, 0), T0)
            .await
            .unwrap();
        let later = T0 + vm::SWEEP_DRAINING_S + 60;
        provider
            .report(&run, &report(VmState::Draining, 0), later)
            .await
            .unwrap();
        assert!(provider.sweep(later + 1).await.unwrap().is_empty());
        assert_eq!(
            provider
                .sweep(later + vm::SWEEP_SILENT_S + 1)
                .await
                .unwrap(),
            vec![run]
        );
    }

    #[tokio::test]
    async fn stop_removes_the_vm() {
        let (provider, _) = setup().await;
        let run = provider.start(request("vm", true)).await.unwrap();
        provider.stop(&run).await.unwrap();
        assert_eq!(provider.capacity("vm").await.unwrap(), Some(Vec::new()));
        assert!(!provider.list(None).await.unwrap()[0].state.is_live());
        assert!(provider.claim(&run).await.is_err());
        assert!(provider.stop("fake-unknown").await.is_err());
    }
}
