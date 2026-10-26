//! Per-pool providers (spec D35, M4 rule R8). The config's `provider` is the main member; each
//! key of `providers` adds a member serving the pools whose `runtime` names it. Run ids and step
//! ids of an added member start with `<runtime>:`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{Config, Pool};
use crate::log::Log;
use crate::types::{
    Capabilities, Outputs, Provider, Rotation, Run, StartRequest, Step, VmCapacity, VmReport,
};
use crate::{Error, Result};

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

    fn capabilities_for(&self, pool: &Pool) -> Capabilities {
        self.for_pool(&pool.tag).0.capabilities_for(pool)
    }

    /// Every member's preflight; the first failure names its runtime.
    async fn preflight(&self) -> Result<()> {
        let members = std::iter::once((self.config.provider.kind.as_str(), &self.main))
            .chain(self.extras.iter().map(|(name, p)| (name.as_str(), p)));
        for (name, provider) in members {
            provider
                .preflight()
                .await
                .map_err(|error| Error::provider(format!("preflight of {name}: {error}")))?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::config::parse_config;
    use crate::types::{Action, Billing, RunState, VmState};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Calls = Arc<Mutex<Vec<String>>>;

    /// Records every call as `<name> <call> <argument>`; run ids and step ids carry `prefix`.
    struct Member {
        name: &'static str,
        prefix: &'static str,
        calls: Calls,
        shared: BTreeMap<String, String>,
        fail_sweep: bool,
        fail_preflight: AtomicBool,
        started: Mutex<Vec<StartRequest>>,
    }

    impl Member {
        fn new(name: &'static str, prefix: &'static str, calls: &Calls) -> Member {
            Member {
                name,
                prefix,
                calls: calls.clone(),
                shared: BTreeMap::new(),
                fail_sweep: false,
                fail_preflight: AtomicBool::new(false),
                started: Mutex::new(Vec::new()),
            }
        }

        fn call(&self, call: &str, argument: &str) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {call} {argument}", self.name));
        }

        fn run_id(&self) -> String {
            format!("{}run-1", self.prefix)
        }
    }

    #[async_trait]
    impl Provider for Member {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                name: self.name.into(),
                docker_daemon: true,
                max_vcpu: 2,
                max_memory_mib: 4096,
                max_storage_mib: 10240,
                max_duration_s: 3600,
                archs: vec!["amd64".into()],
                billing: Billing::PerHour,
            }
        }
        async fn preflight(&self) -> Result<()> {
            self.call("preflight", "");
            if self.fail_preflight.load(Ordering::SeqCst) {
                return Err(Error::provider("no quota"));
            }
            Ok(())
        }
        async fn plan(&self, _rotate: &[Rotation]) -> Result<Vec<Step>> {
            self.call("plan", "");
            Ok(vec![Step::new(
                format!("{}step", self.prefix),
                Action::Create,
                format!("{} resource", self.name),
            )])
        }
        async fn apply(&self, steps: &[Step]) -> Result<Outputs> {
            let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
            self.call("apply", &ids.join(","));
            Ok(steps
                .iter()
                .map(|s| (format!("{}_OUT", s.id), self.name.to_string()))
                .collect())
        }
        async fn destroy(&self) -> Result<()> {
            self.call("destroy", "");
            Ok(())
        }
        async fn start(&self, request: StartRequest) -> Result<String> {
            self.call("start", &request.pool.tag);
            self.started.lock().unwrap().push(request);
            Ok(self.run_id())
        }
        async fn list(&self, pool: Option<&str>) -> Result<Vec<Run>> {
            self.call("list", pool.unwrap_or("*"));
            Ok(vec![Run {
                run_id: self.run_id(),
                pool: pool.unwrap_or("any").to_string(),
                state: RunState::Running,
                created_at: "0".into(),
                labels: BTreeMap::new(),
            }])
        }
        async fn stop(&self, run_id: &str) -> Result<()> {
            self.call("stop", run_id);
            Ok(())
        }
        async fn capacity(&self, pool: &str) -> Result<Option<Vec<VmCapacity>>> {
            self.call("capacity", pool);
            Ok(Some(vec![VmCapacity {
                run_id: self.run_id(),
                state: VmState::Idle,
                free: 1,
                spill: 0,
                seen: None,
                created_at: 0,
            }]))
        }
        async fn claim(&self, run_id: &str) -> Result<()> {
            self.call("claim", run_id);
            Ok(())
        }
        async fn report(&self, run_id: &str, _report: &VmReport, _now: u64) -> Result<()> {
            self.call("report", run_id);
            Ok(())
        }
        async fn shared_env(&self) -> Result<BTreeMap<String, String>> {
            self.call("shared_env", "");
            Ok(self.shared.clone())
        }
        async fn sweep(&self, now: u64) -> Result<Vec<String>> {
            self.call("sweep", &now.to_string());
            if self.fail_sweep {
                return Err(Error::provider("api down"));
            }
            Ok(vec![format!("{}run-old", self.prefix)])
        }
    }

    fn config() -> Arc<Config> {
        Arc::new(
            parse_config(
                r#"{
                  "name": "demo",
                  "gitlab": { "url": "https://gitlab.example", "projectIds": [7] },
                  "host": { "type": "fake" }, "provider": { "type": "fake" },
                  "defaultPool": "vm",
                  "pools": [
                    { "tag": "vm", "image": "img/node", "runtime": "instances" },
                    { "tag": "plain", "image": "img/node", "stages": ["lint"] },
                    { "tag": "hz", "image": "img/node", "runtime": "hetzner", "stages": ["test"] },
                    { "tag": "other", "image": "img/node", "runtime": "other", "stages": ["deploy"] }
                  ],
                  "providers": { "hetzner": { "location": "nbg1" } },
                  "cache": { "type": "s3" }
                }"#,
            )
            .unwrap(),
        )
    }

    struct Setup {
        composite: Composite,
        main: Arc<Member>,
        extra: Arc<Member>,
        calls: Calls,
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl Setup {
        /// The calls since the last look.
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    fn setup(fail_main_sweep: bool, fail_extra_sweep: bool) -> Setup {
        let calls: Calls = Arc::default();
        let mut main = Member::new("main", "", &calls);
        main.shared = BTreeMap::from([
            (
                "CIBURST_AGENT_URL".to_string(),
                "https://bucket.example/ciburst/agent/0a1b".to_string(),
            ),
            ("CACHE_TYPE".to_string(), "s3".to_string()),
        ]);
        main.fail_sweep = fail_main_sweep;
        let mut extra = Member::new("hetzner", "hetzner:", &calls);
        extra.fail_sweep = fail_extra_sweep;
        let (main, extra) = (Arc::new(main), Arc::new(extra));
        let (log, lines) = Log::memory();
        let composite = Composite::new(
            config(),
            main.clone(),
            vec![("hetzner".to_string(), extra.clone() as Arc<dyn Provider>)],
            log,
        );
        Setup {
            composite,
            main,
            extra,
            calls,
            lines,
        }
    }

    fn request(tag: &str) -> StartRequest {
        StartRequest {
            pool: config().pool_by_tag(tag).unwrap().clone(),
            env: BTreeMap::from([
                ("RUNNER_TOKEN".to_string(), "glrt-x".to_string()),
                ("CACHE_TYPE".to_string(), "mine".to_string()),
            ]),
            labels: BTreeMap::new(),
            taken: 2,
        }
    }

    #[tokio::test]
    async fn start_goes_to_the_member_of_the_pools_runtime() {
        let s = setup(false, false);
        for tag in ["vm", "plain", "other"] {
            assert_eq!(s.composite.start(request(tag)).await.unwrap(), "run-1");
        }
        assert_eq!(
            s.calls(),
            ["main start vm", "main start plain", "main start other"],
            "the main member's own pools get no shared env"
        );
        assert!(
            s.main
                .started
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.env == request("vm").env && r.taken == 2)
        );

        assert_eq!(
            s.composite.start(request("hz")).await.unwrap(),
            "hetzner:run-1"
        );
        assert_eq!(s.calls(), ["main shared_env ", "hetzner start hz"]);
        let started = s.extra.started.lock().unwrap();
        assert_eq!(
            started[0].env,
            BTreeMap::from([
                ("RUNNER_TOKEN".to_string(), "glrt-x".to_string()),
                ("CACHE_TYPE".to_string(), "mine".to_string()),
                (
                    "CIBURST_AGENT_URL".to_string(),
                    "https://bucket.example/ciburst/agent/0a1b".to_string()
                ),
            ]),
            "keys already present win"
        );
        assert_eq!(started[0].taken, 2);
    }

    #[tokio::test]
    async fn a_runtime_without_a_member_falls_to_main() {
        let calls: Calls = Arc::default();
        let main = Arc::new(Member::new("main", "", &calls));
        let composite = Composite::new(config(), main, Vec::new(), Log::silent());
        assert_eq!(composite.start(request("hz")).await.unwrap(), "run-1");
        assert_eq!(*calls.lock().unwrap(), ["main start hz"]);
    }

    #[tokio::test]
    async fn list_and_capacity_follow_the_pool_and_list_all_concatenates() {
        let s = setup(false, false);
        let ids = |runs: Vec<Run>| runs.into_iter().map(|r| r.run_id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.composite.list(Some("hz")).await.unwrap()),
            ["hetzner:run-1"]
        );
        assert_eq!(ids(s.composite.list(Some("vm")).await.unwrap()), ["run-1"]);
        assert_eq!(
            ids(s.composite.list(None).await.unwrap()),
            ["run-1", "hetzner:run-1"]
        );
        let capacity = s.composite.capacity("hz").await.unwrap().unwrap();
        assert_eq!(capacity[0].run_id, "hetzner:run-1");
        let capacity = s.composite.capacity("plain").await.unwrap().unwrap();
        assert_eq!(capacity[0].run_id, "run-1");
        assert_eq!(
            s.calls(),
            [
                "hetzner list hz",
                "main list vm",
                "main list *",
                "hetzner list *",
                "hetzner capacity hz",
                "main capacity plain",
            ]
        );
    }

    #[tokio::test]
    async fn run_ids_route_stop_claim_and_report() {
        let s = setup(false, false);
        let report = VmReport {
            state: VmState::Idle,
            free: 2,
            slots: 2,
            spill: 0,
        };
        for (run, member) in [
            ("hetzner:demo-vm-0a1b2c3d", "hetzner"),
            ("hetzner:", "hetzner"),
            ("hetzner-demo", "main"),
            ("hetznerx", "main"),
            ("x:hetzner:y", "main"),
            ("0f6c1a52-7d1e-4b8e-9a43-2f1d5e0c9b11", "main"),
            ("", "main"),
        ] {
            s.composite.stop(run).await.unwrap();
            s.composite.claim(run).await.unwrap();
            s.composite.report(run, &report, 5).await.unwrap();
            assert_eq!(
                s.calls(),
                [
                    format!("{member} stop {run}"),
                    format!("{member} claim {run}"),
                    format!("{member} report {run}"),
                ]
            );
        }
    }

    #[tokio::test]
    async fn sweep_runs_every_member_and_survives_a_failing_one() {
        let cases = [
            (false, false, vec!["run-old", "hetzner:run-old"], vec![]),
            (false, true, vec!["run-old"], vec!["hetzner"]),
            (true, false, vec!["hetzner:run-old"], vec!["main"]),
            (true, true, vec![], vec!["main", "hetzner"]),
        ];
        for (fail_main, fail_extra, expected, failed) in cases {
            let s = setup(fail_main, fail_extra);
            assert_eq!(s.composite.sweep(9).await.unwrap(), expected);
            assert_eq!(s.calls(), ["main sweep 9", "hetzner sweep 9"]);
            let warned: Vec<String> = failed
                .iter()
                .map(|member| format!(r#"warn sweep_failed provider="{member}" error="api down""#))
                .collect();
            assert_eq!(*s.lines.lock().unwrap(), warned);
        }
    }

    #[tokio::test]
    async fn plan_apply_and_destroy_split_by_step_prefix() {
        let s = setup(false, false);
        let mut steps = s.composite.plan(&[]).await.unwrap();
        let ids: Vec<&str> = steps.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, ["step", "hetzner:step"]);
        steps.push(Step::new("other:step", Action::Noop, "unowned"));
        let outputs = s.composite.apply(&steps).await.unwrap();
        assert_eq!(
            outputs,
            Outputs::from([
                ("step_OUT".to_string(), "main".to_string()),
                ("other:step_OUT".to_string(), "main".to_string()),
                ("hetzner:step_OUT".to_string(), "hetzner".to_string()),
            ])
        );
        s.composite.destroy().await.unwrap();
        assert_eq!(
            s.calls(),
            [
                "main plan ",
                "hetzner plan ",
                "main apply step,other:step",
                "hetzner apply hetzner:step",
                "hetzner destroy ",
                "main destroy ",
            ]
        );
    }

    #[tokio::test]
    async fn capabilities_and_shared_env_are_mains() {
        let s = setup(false, false);
        assert_eq!(s.composite.capabilities().name, "main");
        assert_eq!(s.composite.shared_env().await.unwrap(), s.main.shared);
        assert_eq!(s.calls(), ["main shared_env "]);
    }

    #[tokio::test]
    async fn capabilities_for_a_pool_come_from_its_member() {
        let s = setup(false, false);
        let config = config();
        let name = |tag: &str| {
            s.composite
                .capabilities_for(config.pool_by_tag(tag).unwrap())
                .name
        };
        assert_eq!(name("vm"), "main");
        assert_eq!(name("plain"), "main");
        assert_eq!(name("hz"), "hetzner");
        assert_eq!(name("other"), "main", "a runtime without a member");
    }

    #[tokio::test]
    async fn preflight_runs_every_member_and_names_the_failing_runtime() {
        let s = setup(false, false);
        s.composite.preflight().await.unwrap();
        assert_eq!(s.calls(), ["main preflight ", "hetzner preflight "]);

        s.extra.fail_preflight.store(true, Ordering::SeqCst);
        let error = s.composite.preflight().await.unwrap_err().to_string();
        assert_eq!(error, "preflight of hetzner: no quota");
        assert_eq!(s.calls(), ["main preflight ", "hetzner preflight "]);

        s.main.fail_preflight.store(true, Ordering::SeqCst);
        let error = s.composite.preflight().await.unwrap_err().to_string();
        assert_eq!(error, "preflight of fake: no quota");
        assert_eq!(s.calls(), ["main preflight "], "stops at the first failure");
    }
}
