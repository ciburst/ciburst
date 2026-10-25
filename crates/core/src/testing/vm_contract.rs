//! The VM provider contract (spec section 6.1): every provider with a VM runtime runs
//! [`run_vm_provider_contract`] in its tests. A VM starts with `free = slots - taken`, shows in
//! `list` and `capacity`, loses a free slot per claim, takes state, free slots and last seen
//! from a report, is swept by [`crate::vm::sweep_reason`] once silent, and ends on `stop`. Runs
//! of other pools and ids of other runtimes stay untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::cloud_init::{AGENT_SHA256_KEY, AGENT_URL_KEY, DISPATCH_URL, RUNNER_URL_KEY};
use crate::config::{Config, Pool, parse_config};
use crate::provision::keys;
use crate::types::{Provider, StartRequest, VmCapacity, VmReport, VmState};
use crate::vm::{SWEEP_SILENT_S, sweep_reason};

/// Two VM pools of three and two slots, both on the runtime `instances`; a factory moves them
/// to its own runtime in [`VmProviderFactory::config`].
pub const VM_CONTRACT_CONFIG: &str = r#"{
  "name": "vmcontract",
  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
  "host": { "type": "fake" },
  "provider": { "type": "fake" },
  "defaultPool": "vm",
  "pools": [
    { "tag": "vm", "image": "example.invalid/node:24", "stages": ["test"], "runtime": "instances", "slots": 3 },
    { "tag": "other", "image": "example.invalid/node:24", "stages": ["build"], "runtime": "instances", "slots": 2 }
  ],
  "cache": { "type": "s3", "expireDays": 14 }
}"#;

/// Unix seconds at which the contract starts its VMs.
pub const T0: u64 = 1_800_000_000;

const AGENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

pub fn vm_contract_config() -> Config {
    parse_config(VM_CONTRACT_CONFIG).expect("VM contract config")
}

/// Builds VM providers for the contract on a fresh backend each.
#[async_trait]
pub trait VmProviderFactory: Send + Sync {
    type Backend: Send + Sync;
    async fn backend(&self) -> Self::Backend;
    /// The contract config as this provider serves it; by default unchanged.
    fn config(&self, config: Config) -> Config {
        config
    }
    fn make(&self, config: Arc<Config>, backend: &Self::Backend) -> Arc<dyn Provider>;
    /// Unix seconds the backend stamps as creation time of the next VM.
    fn set_clock(&self, backend: &Self::Backend, unix: u64);
    /// A run id in the form of another runtime, which this provider must not act on.
    fn foreign_run_id(&self) -> String {
        "hetzner:vmcontract-vm-0a1b2c3d".to_string()
    }
}

/// Everything a VM start of any provider reads from the request: runner token, VM secret,
/// dispatcher URL, the agent of D38, the runner object of M6 rule R6 and the S3 cache.
pub fn start_env() -> BTreeMap<String, String> {
    [
        ("RUNNER_TOKEN", "glrt-vm-contract"),
        (keys::VM_SECRET, "vm-contract-secret"),
        (DISPATCH_URL, "https://dispatch.example.invalid"),
        (
            AGENT_URL_KEY,
            "https://vmcontract-cache.s3.example.invalid/ciburst/agent/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
        (AGENT_SHA256_KEY, AGENT),
        (
            RUNNER_URL_KEY,
            "https://vmcontract-cache.s3.example.invalid/ciburst/runner/bed4af01315780cf27d259b9d7a74d20cc0a6fc0d34c9952205b59c244639fd1",
        ),
        ("CACHE_TYPE", "s3"),
        ("CACHE_S3_SERVER_ADDRESS", "s3.example.invalid"),
        ("CACHE_S3_BUCKET_NAME", "vmcontract-cache"),
        ("CACHE_S3_BUCKET_LOCATION", "fr-par"),
        ("CACHE_SHARED", "true"),
        ("CACHE_S3_ACCESS_KEY", "CONTRACTCACHEACCESS0"),
        ("CACHE_S3_SECRET_KEY", "contract-cache-secret"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn request(pool: &Pool, taken: u32) -> StartRequest {
    StartRequest {
        pool: pool.clone(),
        env: start_env(),
        labels: BTreeMap::from([("job".to_string(), "42".to_string())]),
        taken,
    }
}

fn report(state: VmState, free: u32, slots: u32) -> VmReport {
    VmReport { state, free, slots }
}

/// A provisioned provider on a fresh backend, its clock at [`T0`].
async fn provisioned<F: VmProviderFactory>(
    factory: &F,
    config: &Arc<Config>,
) -> (F::Backend, Arc<dyn Provider>) {
    let backend = factory.backend().await;
    let provider = factory.make(config.clone(), &backend);
    let steps = provider.plan(&[]).await.expect("plan");
    provider.apply(&steps).await.expect("apply");
    factory.set_clock(&backend, T0);
    (backend, provider)
}

async fn capacity(provider: &dyn Provider, pool: &str) -> Vec<VmCapacity> {
    provider
        .capacity(pool)
        .await
        .expect("capacity")
        .unwrap_or_else(|| panic!("{pool} is a VM pool"))
}

async fn vm(provider: &dyn Provider, pool: &str, run_id: &str) -> VmCapacity {
    capacity(provider, pool)
        .await
        .into_iter()
        .find(|vm| vm.run_id == run_id)
        .unwrap_or_else(|| panic!("{run_id} in the capacity of {pool}"))
}

async fn ids(provider: &dyn Provider, pool: &str) -> BTreeSet<String> {
    capacity(provider, pool)
        .await
        .into_iter()
        .map(|vm| vm.run_id)
        .collect()
}

/// The live runs of `pool` (all pools for `None`).
async fn live(provider: &dyn Provider, pool: Option<&str>) -> BTreeSet<String> {
    provider
        .list(pool)
        .await
        .expect("list")
        .into_iter()
        .filter(|run| run.state.is_live())
        .map(|run| run.run_id)
        .collect()
}

pub async fn run_vm_provider_contract<F: VmProviderFactory>(factory: &F) {
    let config = Arc::new(factory.config(vm_contract_config()));
    let pool = config.pool_by_tag("vm").expect("pool vm").clone();
    let other = config.pool_by_tag("other").expect("pool other").clone();
    assert_eq!(pool.slots, 3, "the contract pool has three slots");

    // start: free = slots - taken, never below zero
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        for (taken, free) in [(2, 1), (0, 3), (1, 2), (3, 0), (5, 0)] {
            let run = provider.start(request(&pool, taken)).await.expect("start");
            assert!(!run.is_empty());
            let started = vm(&*provider, "vm", &run).await;
            assert_eq!(started.free, free, "slots 3, taken {taken}");
            assert_eq!(started.state, VmState::Starting, "taken {taken}");
            assert_eq!(started.seen, None, "taken {taken}");
            assert_eq!(started.created_at, T0, "taken {taken}");
        }
    }

    // list and capacity show the run; runs of other pools stay apart
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let run = provider.start(request(&pool, 1)).await.expect("start vm");
        let run_other = provider
            .start(request(&other, 1))
            .await
            .expect("start other");
        let listed = provider.list(Some("vm")).await.expect("list vm");
        let found = listed.iter().find(|r| r.run_id == run).expect("run listed");
        assert_eq!(found.pool, "vm");
        assert!(found.state.is_live(), "{found:?}");
        assert_eq!(
            live(&*provider, Some("vm")).await,
            BTreeSet::from([run.clone()])
        );
        assert_eq!(
            live(&*provider, Some("other")).await,
            BTreeSet::from([run_other.clone()])
        );
        assert_eq!(
            live(&*provider, None).await,
            BTreeSet::from([run.clone(), run_other.clone()])
        );
        assert_eq!(ids(&*provider, "vm").await, BTreeSet::from([run.clone()]));
        assert_eq!(
            ids(&*provider, "other").await,
            BTreeSet::from([run_other.clone()])
        );
        assert_eq!(vm(&*provider, "other", &run_other).await.free, 1);
    }

    // claim lowers free down to zero and touches no other VM
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let run = provider.start(request(&pool, 1)).await.expect("start");
        let run_other = provider.start(request(&other, 1)).await.expect("start");
        for expected in [1, 0, 0] {
            provider.claim(&run).await.expect("claim");
            assert_eq!(vm(&*provider, "vm", &run).await.free, expected);
        }
        assert_eq!(vm(&*provider, "other", &run_other).await.free, 1);
    }

    // report sets state, free and seen
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let run = provider.start(request(&pool, 1)).await.expect("start");
        let run_other = provider.start(request(&other, 1)).await.expect("start");
        for (state, free, at) in [
            (VmState::Idle, 3, T0 + 60),
            (VmState::Busy, 1, T0 + 70),
            (VmState::Draining, 3, T0 + 80),
        ] {
            provider
                .report(&run, &report(state, free, 3), at)
                .await
                .expect("report");
            let reported = vm(&*provider, "vm", &run).await;
            assert_eq!(
                (reported.state, reported.free, reported.seen),
                (state, free, Some(at))
            );
            assert_eq!(reported.created_at, T0);
        }
        let untouched = vm(&*provider, "other", &run_other).await;
        assert_eq!(
            (untouched.state, untouched.free, untouched.seen),
            (VmState::Starting, 1, None)
        );
    }

    // a silent VM is swept as core::vm::sweep_reason says, a reporting one is kept
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let silent = provider.start(request(&pool, 1)).await.expect("start");
        let reporting = provider.start(request(&pool, 1)).await.expect("start");
        let never = provider.start(request(&pool, 1)).await.expect("start");
        let last = T0 + 100;
        provider
            .report(&silent, &report(VmState::Idle, 3, 3), last)
            .await
            .expect("report");
        provider
            .report(
                &reporting,
                &report(VmState::Idle, 3, 3),
                last + SWEEP_SILENT_S,
            )
            .await
            .expect("report");
        let at_limit = last + SWEEP_SILENT_S;
        assert_eq!(
            provider
                .sweep(at_limit)
                .await
                .expect("sweep")
                .into_iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([never.clone()]),
            "only the VM that never reported is past a limit"
        );
        let now = at_limit + 1;
        let expected: BTreeSet<String> = capacity(&*provider, "vm")
            .await
            .into_iter()
            .filter(|vm| sweep_reason(vm, None, now).is_some())
            .map(|vm| vm.run_id)
            .collect();
        assert_eq!(expected, BTreeSet::from([silent.clone()]));
        let swept: BTreeSet<String> = provider
            .sweep(now)
            .await
            .expect("sweep")
            .into_iter()
            .collect();
        assert_eq!(swept, expected);
        assert_eq!(
            ids(&*provider, "vm").await,
            BTreeSet::from([reporting.clone()])
        );
        assert_eq!(live(&*provider, None).await, BTreeSet::from([reporting]));
    }

    // ids of other runtimes are refused and change nothing
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let run = provider.start(request(&pool, 1)).await.expect("start");
        let foreign = factory.foreign_run_id();
        assert!(provider.claim(&foreign).await.is_err(), "claim {foreign}");
        assert!(
            provider
                .report(&foreign, &report(VmState::Idle, 0, 3), T0 + 5)
                .await
                .is_err(),
            "report {foreign}"
        );
        assert!(provider.stop(&foreign).await.is_err(), "stop {foreign}");
        let kept = vm(&*provider, "vm", &run).await;
        assert_eq!(
            (kept.state, kept.free, kept.seen),
            (VmState::Starting, 2, None)
        );
        assert_eq!(live(&*provider, None).await, BTreeSet::from([run]));
    }

    // stop ends the run; a VM needs the VM secret
    {
        let (_backend, provider) = provisioned(factory, &config).await;
        let run = provider.start(request(&pool, 1)).await.expect("start");
        let kept = provider.start(request(&pool, 1)).await.expect("start");
        provider.stop(&run).await.expect("stop");
        assert_eq!(ids(&*provider, "vm").await, BTreeSet::from([kept.clone()]));
        assert_eq!(live(&*provider, None).await, BTreeSet::from([kept.clone()]));
        let mut without_secret = request(&pool, 1);
        without_secret.env.remove(keys::VM_SECRET);
        assert!(provider.start(without_secret).await.is_err());
        assert_eq!(ids(&*provider, "vm").await, BTreeSet::from([kept]));
    }
}
