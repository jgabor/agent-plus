//! `cargo xtask gaps`: what stands between the scores and the goal, in the
//! order the planner should take it.

use crate::json::quoted;
use crate::{
    Problem, Verdict, accepts, bench, journey, ledger, lock, roadmap, score, testrun, workspace,
};
use std::collections::BTreeMap;

/// A gap isn't a failure of this command. A broken spec quote is, as in
/// `score`, because the list can't be trusted while one exists. So is a
/// promise that only a `t2` check proves.
pub fn run(json: bool) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    // `gaps` runs no step that spends, nor one that needs such a step, so the
    // nightly job's last run says whether one passes. Only then does `gaps`
    // need GitHub, and it asks first, so a run that can't reach it stops early.
    let details = match journey::left_out(&root)? {
        0 => String::new(),
        _ => roadmap::last_details(&root)?,
    };
    let mut hold = lock::testing(&root)?;
    let results = testrun::run(&root, &mut hold)?;
    let score = score::compute(&root, &results)?;
    score::report_findings(&score);
    let verdict = if score.findings.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let outcomes = journey::outcomes(&root, &journey::Keys::new())?;
    drop(hold);
    let values = ledger::at_head(&root)?.map_or_else(BTreeMap::new, |record| record.budgets);
    let budgets = bench::read_budgets(&root)?;

    let passed =
        |o: &journey::Outcome| o.state == "pass" || (o.spends && roadmap::passed_in(&details, o));
    let steps: Vec<_> = outcomes.iter().filter(|o| !passed(o)).collect();
    // The planner works on one milestone at a time: the first in roadmap
    // order that has a step still to pass.
    let milestone = journey::milestones(&root)?
        .into_iter()
        .map(|milestone| milestone.name)
        .find(|name| steps.iter().any(|step| step.milestone == *name));
    let missed = accepts::missed(&budgets, &values);
    if json {
        let steps: Vec<String> = steps
            .iter()
            .map(|step| {
                format!(
                    "{{\"id\":{},\"name\":{},\"milestone\":{},\"state\":{}}}",
                    step.id,
                    quoted(&step.name),
                    quoted(&step.milestone),
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
            "{{\"milestone\":{},\"steps\":[{}],\"budgets\":[{}],\"promises\":[{}]}}",
            milestone.map_or("null".to_string(), |name| quoted(&name)),
            steps.join(","),
            missed.join(","),
            promises.join(",")
        );
        return Ok(verdict);
    }
    if let Some(name) = milestone {
        println!("milestone {name}");
    }
    for step in steps {
        println!(
            "step {} {} {}: {}",
            step.id, step.state, step.milestone, step.name
        );
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
    Ok(verdict)
}
