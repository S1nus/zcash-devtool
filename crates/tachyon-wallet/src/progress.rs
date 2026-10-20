//! Opt-in proof execution events, separate from diagnostic logging.
//! No witnesses, wallet identifiers or inputs are logged; output is stderr only.
use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
    sync::{Mutex, OnceLock},
    time::Instant,
};

pub(crate) type StepId = u64;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Mode {
    #[default]
    Off,
    Plain,
    Tty,
    Auto,
}
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Lifecycle {
    Started,
    Completed,
    Failed,
    Interrupted,
}
impl Lifecycle {
    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}
struct State {
    mode: Mode,
    next: StepId,
    active: BTreeSet<StepId>,
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            mode: Mode::Off,
            next: 1,
            active: BTreeSet::new(),
        })
    })
}
pub(crate) fn configure(mode: Mode) {
    let mode = match mode {
        Mode::Auto | Mode::Tty if io::stderr().is_terminal() => Mode::Tty,
        Mode::Auto | Mode::Tty => Mode::Plain,
        mode => mode,
    };
    state().lock().unwrap_or_else(|e| e.into_inner()).mode = mode;
}
fn render(
    mode: Mode,
    id: StepId,
    name: &str,
    deps: &[StepId],
    lifecycle: Lifecycle,
    elapsed_ms: Option<u128>,
    active: &BTreeSet<StepId>,
) -> String {
    if mode == Mode::Plain {
        let mut event = serde_json::json!({
            "type": "proof_progress", "step_id": id, "operation": name,
            "dependencies": deps, "state": lifecycle,
        });
        if let Some(elapsed) = elapsed_ms {
            event["elapsed_ms"] = serde_json::json!(elapsed);
        }
        format!("{event}\n")
    } else {
        let duration = elapsed_ms
            .map(|ms| format!(" ({ms}ms)"))
            .unwrap_or_default();
        format!(
            "proof #{id} {name}: {}{duration} deps={deps:?} active={active:?}\n",
            lifecycle.label()
        )
    }
}
impl State {
    fn event(
        &mut self,
        id: StepId,
        name: &str,
        deps: &[StepId],
        lifecycle: Lifecycle,
        elapsed_ms: Option<u128>,
    ) {
        if lifecycle == Lifecycle::Started {
            self.active.insert(id);
        } else {
            self.active.remove(&id);
        }
        if self.mode != Mode::Off {
            // Diagnostic failure must never turn a successful payment into a failed one.
            let _ = io::stderr().lock().write_all(
                render(
                    self.mode,
                    id,
                    name,
                    deps,
                    lifecycle,
                    elapsed_ms,
                    &self.active,
                )
                .as_bytes(),
            );
        }
    }
}
pub(crate) struct Step {
    id: StepId,
    name: &'static str,
    deps: Vec<StepId>,
    start: Instant,
    finished: bool,
}
impl Step {
    pub(crate) fn start(name: &'static str, deps: &[StepId]) -> Self {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next;
        state.next += 1;
        let start = Instant::now();
        state.event(id, name, deps, Lifecycle::Started, None);
        Self {
            id,
            name,
            deps: deps.to_vec(),
            start,
            finished: false,
        }
    }
    pub(crate) fn id(&self) -> StepId {
        self.id
    }
    fn finish(&mut self, lifecycle: Lifecycle) {
        state().lock().unwrap_or_else(|e| e.into_inner()).event(
            self.id,
            self.name,
            &self.deps,
            lifecycle,
            Some(self.start.elapsed().as_millis()),
        );
        self.finished = true;
    }
    pub(crate) fn run<T, E>(mut self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let result = f();
        self.finish(if result.is_ok() {
            Lifecycle::Completed
        } else {
            Lifecycle::Failed
        });
        result
    }
}
impl Drop for Step {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Lifecycle::Interrupted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendering_escapes_json_and_omits_start_duration() {
        let line = render(
            Mode::Plain,
            4,
            "quoted\"name",
            &[2, 3],
            Lifecycle::Started,
            None,
            &BTreeSet::new(),
        );
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["operation"], "quoted\"name");
        assert_eq!(event["dependencies"], serde_json::json!([2, 3]));
        assert!(event.get("elapsed_ms").is_none());
        assert!(!line.contains('\u{1b}'));
        let line = render(
            Mode::Tty,
            5,
            "NullifierDerive",
            &[2],
            Lifecycle::Failed,
            Some(12),
            &BTreeSet::from([3]),
        );
        assert!(line.contains("failed (12ms) deps=[2] active={3}"));
    }
    #[test]
    fn overlapping_steps_and_failures_keep_independent_ids() {
        let a = Step::start("test-a", &[]);
        let b = Step::start("test-b", &[]);
        assert_ne!(a.id(), b.id());
        {
            let state = state().lock().unwrap();
            assert!(state.active.contains(&a.id()));
            assert!(state.active.contains(&b.id()));
        }
        let a_id = a.id();
        assert!(a.run(|| Err::<(), _>("deliberate failure")).is_err());
        assert!(!state().lock().unwrap().active.contains(&a_id));
        let b_id = b.id();
        drop(b);
        assert!(!state().lock().unwrap().active.contains(&b_id));
    }
    #[test]
    fn stream_contract() -> anyhow::Result<()> {
        const CHILD: &str = "TACHYON_TRACE_TEST_MODE";
        if let Ok(mode) = std::env::var(CHILD) {
            use clap::ValueEnum;
            configure(Mode::from_str(&mode, false).map_err(anyhow::Error::msg)?);
            let mut blocks = vec![crate::tests::genesis()];
            let receipt = crate::tests::own(100_000);
            let transaction = crate::tests::shield(&blocks, &receipt)?;
            crate::tests::mine(&mut blocks, Some(&transaction))?;
            let key = zcash_tachyon::keys::private::SpendingKey::from(crate::tests::SEED);
            let _proof = crate::proofs::spendable(&blocks, &key, receipt.note()?)?;
            assert!(
                Step::start("test-failure", &[])
                    .run(|| Err::<(), _>("private diagnostic"))
                    .is_err()
            );
            drop(Step::start("test-drop", &[]));
            println!("{{\"preview\":\"mock-prover\",\"status\":\"ok\"}}");
            return Ok(());
        }
        for mode in ["off", "plain", "auto", "tty"] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "progress::tests::stream_contract",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, mode)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout)?;
            assert!(stdout.contains("{\"preview\":\"mock-prover\",\"status\":\"ok\"}"));
            assert!(!stdout.contains("proof_progress"));
            let stderr = String::from_utf8(output.stderr)?;
            assert!(!stderr.contains("private diagnostic"));
            if mode == "off" {
                assert!(stderr.is_empty());
                continue;
            }
            let mut completed = BTreeSet::new();
            let mut active = BTreeSet::new();
            let mut operations = BTreeSet::new();
            let mut outcomes = BTreeSet::new();
            for line in stderr.lines() {
                let event: serde_json::Value = serde_json::from_str(line)?;
                let id = event["step_id"].as_u64().unwrap();
                operations.insert(event["operation"].as_str().unwrap().to_owned());
                for dep in event["dependencies"].as_array().unwrap() {
                    assert!(completed.contains(&dep.as_u64().unwrap()));
                }
                let lifecycle = event["state"].as_str().unwrap();
                outcomes.insert(lifecycle.to_owned());
                if lifecycle == "started" {
                    assert!(active.insert(id));
                    assert!(event.get("elapsed_ms").is_none());
                } else {
                    assert!(active.remove(&id));
                    assert!(event["elapsed_ms"].is_number());
                    if lifecycle == "completed" {
                        completed.insert(id);
                    }
                }
            }
            assert!(active.is_empty());
            for name in ["NoteSeed", "NullifierDerive", "SpendableInit"] {
                assert!(operations.contains(name));
            }
            for outcome in ["started", "completed", "failed", "interrupted"] {
                assert!(outcomes.contains(outcome));
            }
        }
        Ok(())
    }
}
