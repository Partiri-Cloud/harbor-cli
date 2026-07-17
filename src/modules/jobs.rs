//! `partiri service jobs` — list the deploy/lifecycle jobs for the current service.

use crate::client::ApiClient;
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::output::{colored_job_status, ctx, format_datetime, print_table, JobRow};

/// Entry point for `partiri service jobs`. Lists jobs newest-first: the top 5 in
/// human mode, the full list in JSON mode.
pub fn run_list(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;
    let mut jobs = client.list_service_jobs(id)?;
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Plain mode renders only the top 5 (table compactness); JSON mode returns the full list.
    let take_n = if ctx().json { jobs.len() } else { 5 };

    let rows: Vec<JobRow> = jobs
        .into_iter()
        .take(take_n)
        .map(|j| JobRow {
            job_type: j.job_type,
            deploy_ref: j
                .deploy_ref
                .as_deref()
                .map(|r| r.get(..7).unwrap_or(r).to_string())
                .unwrap_or_else(|| "—".to_string()),
            status: if ctx().json {
                j.status.clone()
            } else {
                colored_job_status(&j.status)
            },
            created_at: j
                .created_at
                .as_deref()
                .map(format_datetime)
                .unwrap_or_else(|| "—".to_string()),
        })
        .collect();

    if rows.is_empty() && !ctx().json {
        println!("No jobs found for this service.");
        return Ok(());
    }

    print_table(rows);
    Ok(())
}
