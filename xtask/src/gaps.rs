//! `cargo xtask gaps`: what stands between the scores and the goal, in the
//! order the planner should take it.

use crate::json::quoted;
use crate::{Problem, Verdict, bench, journey, ledger, lock, score, testrun, workspace};
use std::collections::BTreeMap;

/// Exits 0 whenever it could run, because a gap isn't a failure of this
/// command. A problem in the spec's quotes goes to stderr, as `score` puts it.
pub fn run(json: bool) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let hold = lock::shared(&root)?;
    let results = testrun::run(&root, &hold)?;
    let score = score::compute(&root, &results)?;
    score::report_findings(&score);
    let outcomes = journey::outcomes(&root)?;
    drop(hold);
    let values = ledger::at_head(&root)?.map_or_else(BTreeMap::new, |record| record.budgets);
    let budgets = bench::read_budgets(&root)?;

    let steps: Vec<_> = outcomes.iter().filter(|o| o.state != "pass").collect();
    let missed = bench::missed(&budgets, &values);
    if json {
        let steps: Vec<String> = steps
            .iter()
            .map(|step| {
                format!(
                    "{{\"id\":{},\"name\":{},\"state\":{}}}",
                    step.id,
                    quoted(&step.name),
                    quoted(step.state)
                )
            })
            .collect();
        let missed: Vec<String> = missed
            .iter()
            .map(|budget| {
                format!(
                    "{{\"key\":{},\"name\":{},\"target\":{},\"value\":{}}}",
                    quoted(budget.key),
                    quoted(budget.name),
                    budget.target,
                    budget
                        .value
                        .map_or("null".to_string(), |value| value.to_string())
                )
            })
            .collect();
        let promises: Vec<String> = score
            .unproven
            .iter()
            .map(|(heading, sentence)| {
                format!(
                    "{{\"heading\":{},\"sentence\":{}}}",
                    quoted(heading),
                    quoted(sentence)
                )
            })
            .collect();
        println!(
            "{{\"steps\":[{}],\"budgets\":[{}],\"promises\":[{}]}}",
            steps.join(","),
            missed.join(","),
            promises.join(",")
        );
        return Ok(Verdict::Pass);
    }
    for step in steps {
        println!("step {} {} {}", step.id, step.state, step.name);
    }
    for budget in missed {
        let value = budget
            .value
            .map_or("none".to_string(), |value| value.to_string());
        println!(
            "budget {} {value} of {} {}",
            budget.key, budget.target, budget.name
        );
    }
    for (heading, sentence) in &score.unproven {
        println!("promise {heading}: {sentence}");
    }
    Ok(Verdict::Pass)
}
