//! `POST /api/phantom_route_plan` — ask the delegation router how it would
//! staff a piece of work, without delegating anything.
//!
//! Body: `{ "tasks": [{ "label"?, "task_kind", "difficulty" }] }`. Answer: one
//! entry per task with the agent, model and effort `delegate_to_agent`
//! (`agent_type: "auto"`) would get right now, plus the router's reason. It is
//! the same `DbRouting::plan` a real delegation runs, so quota, the active
//! profile and what the router has learned all count. Advisory: easy tasks keep
//! their small exploration draw, so two calls may differ on those.
//!
//! Lives in the web layer only (`bin/phantom-plan` calls it), so any Claude
//! session on the machine can consult the router, not just Phantom's own.

use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::{Deserialize, Serialize};

use crate::acp::delegation::router::{Difficulty, TaskKind};
use crate::acp::delegation::routing::{DbRouting, DelegationRouting, RoutingArgs};
use crate::app_error::AppCommandError;
use crate::app_state::AppState;

/// More than this is not a plan, it is a mistake.
const MAX_TASKS: usize = 40;

#[derive(Deserialize)]
pub struct PlanTask {
    #[serde(default)]
    label: String,
    task_kind: String,
    difficulty: String,
}

#[derive(Deserialize)]
pub struct PlanRequest {
    tasks: Vec<PlanTask>,
}

#[derive(Serialize, Default)]
pub struct PlannedTask {
    label: String,
    task_kind: String,
    difficulty: String,
    agent_type: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    reason: Option<String>,
    error: Option<String>,
}

fn parse(task: &PlanTask) -> Result<(TaskKind, Difficulty), String> {
    let kind = TaskKind::parse(&task.task_kind).ok_or_else(|| {
        let known: Vec<&str> = TaskKind::ALL.iter().map(TaskKind::as_str).collect();
        format!("task_kind \"{}\" no existe; valen: {}", task.task_kind, known.join(", "))
    })?;
    let difficulty = Difficulty::parse(&task.difficulty).ok_or_else(|| {
        let known: Vec<&str> = Difficulty::ALL.iter().map(Difficulty::as_str).collect();
        format!("difficulty \"{}\" no existe; valen: {}", task.difficulty, known.join(", "))
    })?;
    Ok((kind, difficulty))
}

pub async fn phantom_route_plan(
    Extension(state): Extension<Arc<AppState>>,
    Json(req): Json<PlanRequest>,
) -> Result<Json<Vec<PlannedTask>>, AppCommandError> {
    let routing = DbRouting::new(state.db.conn.clone());
    let mut out = Vec::with_capacity(req.tasks.len().min(MAX_TASKS));
    for task in req.tasks.iter().take(MAX_TASKS) {
        let mut planned = PlannedTask {
            label: task.label.clone(),
            task_kind: task.task_kind.clone(),
            difficulty: task.difficulty.clone(),
            ..Default::default()
        };
        match parse(task) {
            Err(e) => planned.error = Some(e),
            Ok((kind, difficulty)) => {
                let args = RoutingArgs {
                    auto: true,
                    task_kind: Some(kind),
                    difficulty: Some(difficulty),
                    ..Default::default()
                };
                match routing.plan(&args).await {
                    Ok(plan) => {
                        planned.agent_type = Some(
                            serde_json::to_value(plan.agent_type)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_string))
                                .unwrap_or_default(),
                        );
                        planned.model = plan.model;
                        planned.effort = plan.effort;
                        planned.reason = plan.info.map(|i| i.reason);
                    }
                    Err(e) => planned.error = Some(e),
                }
            }
        }
        out.push(planned);
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(kind: &str, difficulty: &str) -> PlanTask {
        PlanTask { label: String::new(), task_kind: kind.into(), difficulty: difficulty.into() }
    }

    #[test]
    fn known_kinds_parse_and_unknown_ones_say_what_is_valid() {
        assert!(parse(&task("implement", "hard")).is_ok());
        assert!(parse(&task(" Debug ", "LONG")).is_ok());
        let e = parse(&task("arreglar", "hard")).unwrap_err();
        assert!(e.contains("arreglar") && e.contains("implement") && e.contains("debug"));
        let e = parse(&task("plan", "enorme")).unwrap_err();
        assert!(e.contains("enorme") && e.contains("trivial") && e.contains("long"));
    }
}
