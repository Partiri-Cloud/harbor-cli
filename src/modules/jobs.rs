//! `partiri service jobs` — list the deploy/lifecycle jobs for the current service.

use crate::client::{ApiClient, Job};
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::output::{colored_job_status, ctx, format_datetime, print_table, JobRow, JobTriggerRow};

/// The short label for a job trigger as the API reports it.
pub(crate) fn trigger_label(trigger: Option<&str>) -> &'static str {
    match trigger {
        Some("github_push") => "push",
        Some("manual") => "manual",
        Some("preview") => "preview",
        Some("deploy_hook") => "deploy hook",
        Some("rollback") => "rollback",
        _ => "—",
    }
}

/// Whether the human table should carry a Trigger column: only when a GitHub
/// push or a preview started some job, so services that never used the GitHub
/// App keep the table they always had.
pub(crate) fn shows_trigger_column(jobs: &[Job]) -> bool {
    jobs.iter()
        .any(|j| matches!(j.trigger.as_deref(), Some("github_push" | "preview")))
}

fn short_ref(j: &Job) -> String {
    j.deploy_ref
        .as_deref()
        .map(|r| r.get(..7).unwrap_or(r).to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn status_of(j: &Job) -> String {
    if ctx().json {
        j.status.clone()
    } else {
        colored_job_status(&j.status)
    }
}

fn created_of(j: &Job) -> String {
    j.created_at
        .as_deref()
        .map(format_datetime)
        .unwrap_or_else(|| "—".to_string())
}

/// Entry point for `partiri service jobs`. Lists jobs newest-first: the top 5 in
/// human mode, the full list in JSON mode.
pub fn run_list(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    let mut jobs = client.list_service_jobs(id)?;
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    if jobs.is_empty() && !ctx().json {
        println!("No jobs found for this service.");
        return Ok(());
    }

    if !ctx().json && shows_trigger_column(&jobs) {
        let rows: Vec<JobTriggerRow> = jobs
            .iter()
            .take(5)
            .map(|j| JobTriggerRow {
                job_type: j.job_type.clone(),
                deploy_ref: short_ref(j),
                trigger: trigger_label(j.trigger.as_deref()).to_string(),
                status: status_of(j),
                created_at: created_of(j),
            })
            .collect();
        print_table(rows);
        return Ok(());
    }

    // Plain mode renders only the top 5 (table compactness); JSON mode returns the full list.
    let take_n = if ctx().json { jobs.len() } else { 5 };
    let rows: Vec<JobRow> = jobs
        .into_iter()
        .take(take_n)
        .map(|j| JobRow {
            deploy_ref: short_ref(&j),
            status: status_of(&j),
            created_at: created_of(&j),
            trigger: j.trigger,
            job_type: j.job_type,
        })
        .collect();

    print_table(rows);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(trigger: Option<&str>) -> Job {
        serde_json::from_value(json!({
            "id": "j", "fk_service": "s", "type": "deploy", "status": "succeeded",
            "trigger": trigger
        }))
        .unwrap()
    }

    #[test]
    fn labels_cover_every_trigger() {
        assert_eq!(trigger_label(Some("github_push")), "push");
        assert_eq!(trigger_label(Some("manual")), "manual");
        assert_eq!(trigger_label(Some("preview")), "preview");
        assert_eq!(trigger_label(Some("deploy_hook")), "deploy hook");
        assert_eq!(trigger_label(Some("rollback")), "rollback");
        assert_eq!(trigger_label(None), "—");
        assert_eq!(trigger_label(Some("surprise")), "—");
    }

    #[test]
    fn trigger_column_appears_only_for_push_or_preview_jobs() {
        assert!(!shows_trigger_column(&[
            job(None),
            job(Some("manual")),
            job(Some("deploy_hook"))
        ]));
        assert!(shows_trigger_column(&[
            job(Some("manual")),
            job(Some("github_push"))
        ]));
        assert!(shows_trigger_column(&[job(Some("preview"))]));
    }

    #[test]
    fn json_rows_carry_the_trigger_only_when_the_api_sent_one() {
        let with = JobRow {
            job_type: "deploy".into(),
            deploy_ref: "—".into(),
            status: "ok".into(),
            created_at: "—".into(),
            trigger: Some("github_push".into()),
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap()["trigger"],
            "github_push"
        );
        let without = JobRow {
            trigger: None,
            ..with
        };
        assert!(serde_json::to_value(&without)
            .unwrap()
            .get("trigger")
            .is_none());
    }
}
