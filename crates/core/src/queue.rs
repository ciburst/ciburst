//! Queue first (spec D53, M6.2 rules R4, R5 and R7): the start rule that decides which queued VM
//! jobs wait for a paid slot and how many VMs to buy, and the report interval a VM gets.

use crate::ledger::Queued;

/// M6.2 rule R5: queued entries older than this count as gone, a lost job event.
pub const QUEUED_MAX_AGE_S: u64 = 1800;
/// M6.2 rule R8: a spilled lane job that no VM slot took within this many seconds gets a lane
/// run.
pub const SPILL_WAIT_S: u64 = 20;
/// M6.2 rule R8: a VM's spill slots count only from a report this recent.
pub const SPILL_FRESH_S: u64 = 30;
/// M6.2 rule R7: the report interval while idle slots spill or VM jobs queue.
pub const REPORT_QUEUE_S: u64 = 15;
/// M6.2 rule R7: the report interval of a busy VM.
pub const REPORT_BUSY_S: u64 = 30;
/// M6.2 rule R7: the report interval otherwise.
pub const REPORT_IDLE_S: u64 = 600;

/// A running VM job of the pool as the start rule sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningJob {
    /// Unix seconds the job started.
    pub started: u64,
    /// The typical duration of its name, `None` before a first finished run.
    pub typical: Option<u64>,
}

/// What the start rule of one VM pool reads (M6.2 rule R4).
#[derive(Debug, Clone)]
pub struct RuleInput<'a> {
    pub now: u64,
    pub queued: &'a [Queued],
    pub running: &'a [RunningJob],
    /// Free slots that take queued jobs as soon as they are up: those of VMs in state
    /// `starting` younger than 10 minutes, and those of the claim candidates (M4 rule R1).
    pub ready: u32,
    pub slots: u32,
    pub max_queue_s: u64,
    pub max_runs: u32,
    /// VMs of the pool that are not draining.
    pub live: usize,
}

/// The start rule's verdict on one queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub job: String,
    /// Seconds until the job expects a slot, `None` when no slot is expected.
    pub wait_s: Option<u64>,
    pub needs_vm: bool,
}

/// The start rule's result: per queued job, oldest first, its verdict, and the VMs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePlan {
    pub verdicts: Vec<Verdict>,
    pub needing: usize,
    pub start: u32,
}

impl RulePlan {
    pub fn verdict(&self, job: &str) -> Option<&Verdict> {
        self.verdicts.iter().find(|v| v.job == job)
    }
}

/// M6.2 rule R4, steps 1 to 6:
/// 1. each running job frees its slot after `max(0, typical - (now - started))`, 0 without a
///    typical duration; ascending;
/// 2. queued jobs oldest first, by `at`, then job id (numbers before other ids);
/// 3. the `ready` slots take the oldest queued jobs, which expect a slot at once;
/// 4. each further queued job takes the next running job's slot after its remainder; a job left
///    over expects no slot;
/// 5. a job needs a VM when it expects no slot or when `(now - at) + wait > maxQueueS`;
/// 6. `ceil(needing / slots)` VMs start, at most `maxRuns` minus the live VMs.
pub fn start_rule(input: &RuleInput<'_>) -> RulePlan {
    let now = input.now;
    let mut remaining: Vec<u64> = input
        .running
        .iter()
        .map(|job| {
            job.typical
                .map(|typical| typical.saturating_sub(now.saturating_sub(job.started)))
                .unwrap_or(0)
        })
        .collect();
    remaining.sort_unstable();
    let mut queued: Vec<&Queued> = input.queued.iter().collect();
    queued.sort_by(|a, b| (a.at, job_key(&a.job)).cmp(&(b.at, job_key(&b.job))));
    let ready = input.ready as usize;
    let mut slots = remaining.into_iter();
    let verdicts: Vec<Verdict> = queued
        .iter()
        .enumerate()
        .map(|(index, job)| {
            let wait_s = if index < ready { Some(0) } else { slots.next() };
            let needs_vm = match wait_s {
                None => true,
                Some(wait) => now.saturating_sub(job.at) + wait > input.max_queue_s,
            };
            Verdict {
                job: job.job.clone(),
                wait_s,
                needs_vm,
            }
        })
        .collect();
    let needing = verdicts.iter().filter(|v| v.needs_vm).count();
    let wanted = needing.div_ceil(input.slots.max(1) as usize);
    let room = (input.max_runs as usize).saturating_sub(input.live);
    RulePlan {
        verdicts,
        needing,
        start: u32::try_from(wanted.min(room)).unwrap_or(u32::MAX),
    }
}

/// A job id in the queue's order: numbers by value before other ids by text.
fn job_key(job: &str) -> (u8, u64, &str) {
    match job.parse::<u64>() {
        Ok(number) => (0, number, ""),
        Err(_) => (1, 0, job),
    }
}

/// M6.2 rule R7: 15 s while idle slots spill or VM jobs queue, 30 s while the VM is busy, 600 s
/// otherwise.
pub fn next_report_s(spill: bool, queued: bool, busy: bool) -> u64 {
    if spill || queued {
        REPORT_QUEUE_S
    } else if busy {
        REPORT_BUSY_S
    } else {
        REPORT_IDLE_S
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn queued(job: &str, ago: u64) -> Queued {
        Queued {
            job: job.into(),
            name: format!("name-{job}"),
            pipeline: "9".into(),
            at: NOW - ago,
        }
    }

    /// A job that started `ago` seconds before [`NOW`] with typical duration `typical`.
    fn busy(ago: u64, typical: Option<u64>) -> RunningJob {
        RunningJob {
            started: NOW - ago,
            typical,
        }
    }

    fn input<'a>(queued: &'a [Queued], running: &'a [RunningJob]) -> RuleInput<'a> {
        RuleInput {
            now: NOW,
            queued,
            running,
            ready: 0,
            slots: 2,
            max_queue_s: 90,
            max_runs: 4,
            live: 1,
        }
    }

    /// `(job, wait, needs a VM)` per verdict.
    fn verdicts(plan: &RulePlan) -> Vec<(&str, Option<u64>, bool)> {
        plan.verdicts
            .iter()
            .map(|v| (v.job.as_str(), v.wait_s, v.needs_vm))
            .collect()
    }

    #[test]
    fn a_chain_waits_for_the_slot_that_frees_in_two_seconds() {
        let running = [busy(58, Some(60))];
        let queued = [queued("2", 0)];
        let plan = start_rule(&input(&queued, &running));
        assert_eq!(verdicts(&plan), [("2", Some(2), false)]);
        assert_eq!((plan.needing, plan.start), (0, 0));
    }

    #[test]
    fn a_long_build_ahead_starts_a_vm() {
        let running = [busy(200, Some(600))];
        let queued = [queued("2", 0)];
        let plan = start_rule(&input(&queued, &running));
        assert_eq!(verdicts(&plan), [("2", Some(400), true)]);
        assert_eq!((plan.needing, plan.start), (1, 1));
    }

    #[test]
    fn the_wait_counts_from_the_pending_event() {
        let running = [busy(10, Some(100))];
        for (ago, needs) in [(0, false), (1, true), (60, true)] {
            let queued = [queued("2", ago)];
            let plan = start_rule(&input(&queued, &running));
            assert_eq!(plan.verdicts[0].wait_s, Some(90), "{ago}");
            assert_eq!(plan.verdicts[0].needs_vm, needs, "{ago}: 90 is the limit");
        }
        let overdue = [busy(500, Some(100))];
        let queued = [queued("2", 30)];
        let plan = start_rule(&input(&queued, &overdue));
        assert_eq!(
            verdicts(&plan),
            [("2", Some(0), false)],
            "an overdue job frees its slot any moment"
        );
    }

    #[test]
    fn unknown_durations_queue_until_max_queue_s_passed() {
        let running = [busy(5, None), busy(300, None)];
        for (ago, needs) in [(0, false), (90, false), (91, true)] {
            let queued = [queued("7", ago)];
            let plan = start_rule(&input(&queued, &running));
            assert_eq!(verdicts(&plan), [("7", Some(0), needs)], "{ago}");
            assert_eq!(plan.start, u32::from(needs), "{ago}");
        }
    }

    #[test]
    fn a_job_left_over_expects_no_slot_and_needs_a_vm_at_once() {
        let running = [busy(10, Some(20))];
        let queued = [queued("1", 5), queued("2", 0)];
        let plan = start_rule(&input(&queued, &running));
        assert_eq!(verdicts(&plan), [("1", Some(10), false), ("2", None, true)]);
        assert_eq!((plan.needing, plan.start), (1, 1));
        let nothing = start_rule(&input(&queued, &[]));
        assert_eq!(verdicts(&nothing), [("1", None, true), ("2", None, true)]);
        assert_eq!(nothing.start, 1, "two jobs fit one VM of two slots");
    }

    #[test]
    fn starting_vms_absorb_the_oldest_queued_jobs() {
        let running = [busy(10, Some(600))];
        let queued = [queued("3", 0), queued("1", 20), queued("2", 20)];
        let mut ready = input(&queued, &running);
        ready.ready = 2;
        let plan = start_rule(&ready);
        assert_eq!(
            verdicts(&plan),
            [
                ("1", Some(0), false),
                ("2", Some(0), false),
                ("3", Some(590), true)
            ],
            "by time, then job number"
        );
        assert_eq!(plan.start, 1);
        ready.ready = 3;
        assert_eq!(start_rule(&ready).start, 0);
    }

    #[test]
    fn running_jobs_free_their_slots_shortest_first() {
        let running = [busy(10, Some(400)), busy(30, Some(40)), busy(0, None)];
        let queued = [queued("1", 0), queued("2", 0), queued("3", 0)];
        let plan = start_rule(&input(&queued, &running));
        assert_eq!(
            verdicts(&plan),
            [
                ("1", Some(0), false),
                ("2", Some(10), false),
                ("3", Some(390), true)
            ]
        );
    }

    #[test]
    fn max_runs_caps_the_vms_to_start() {
        let queued: Vec<Queued> = (1..=9).map(|job| queued(&job.to_string(), 0)).collect();
        let mut capped = input(&queued, &[]);
        capped.slots = 1;
        assert_eq!(start_rule(&capped).start, 3, "4 runs, 1 live");
        capped.live = 4;
        let plan = start_rule(&capped);
        assert_eq!((plan.needing, plan.start), (9, 0));
        capped.live = 6;
        assert_eq!(start_rule(&capped).start, 0, "more live than maxRuns");
    }

    #[test]
    fn three_jobs_needing_vms_of_two_slots_start_two() {
        let queued = [queued("1", 0), queued("2", 0), queued("3", 0)];
        let plan = start_rule(&input(&queued, &[]));
        assert_eq!((plan.needing, plan.start), (3, 2));
        let mut one_slot = input(&queued, &[]);
        one_slot.slots = 1;
        assert_eq!(start_rule(&one_slot).start, 3);
        one_slot.slots = 0;
        assert_eq!(start_rule(&one_slot).start, 3, "slots 0 counts as 1");
    }

    #[test]
    fn max_queue_s_zero_buys_a_vm_for_any_wait() {
        let running = [busy(58, Some(60))];
        let queued = [queued("2", 0)];
        let mut eager = input(&queued, &running);
        eager.max_queue_s = 0;
        assert_eq!(verdicts(&start_rule(&eager)), [("2", Some(2), true)]);
        let mut ready = input(&queued, &[]);
        ready.max_queue_s = 0;
        ready.ready = 1;
        assert_eq!(
            verdicts(&start_rule(&ready)),
            [("2", Some(0), false)],
            "a ready slot at once is no wait"
        );
    }

    #[test]
    fn an_empty_queue_starts_nothing() {
        let running = [busy(1, Some(5))];
        let plan = start_rule(&input(&[], &running));
        assert_eq!(
            plan,
            RulePlan {
                verdicts: Vec::new(),
                needing: 0,
                start: 0
            }
        );
    }

    #[test]
    fn text_ids_order_after_numbers() {
        let queued = [
            queued("b", 0),
            queued("10", 0),
            queued("9", 0),
            queued("a", 0),
        ];
        let plan = start_rule(&input(&queued, &[]));
        let order: Vec<&str> = plan.verdicts.iter().map(|v| v.job.as_str()).collect();
        assert_eq!(order, ["9", "10", "a", "b"]);
        assert!(plan.verdict("a").is_some() && plan.verdict("c").is_none());
    }

    #[test]
    fn reports_come_every_15_s_while_spilling_or_queued() {
        assert_eq!(next_report_s(true, false, false), 15);
        assert_eq!(next_report_s(false, true, true), 15);
        assert_eq!(next_report_s(false, false, true), 30);
        assert_eq!(next_report_s(false, false, false), 600);
    }
}
