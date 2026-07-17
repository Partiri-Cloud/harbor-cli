//! `partiri service metrics` — show live CPU/RAM/network sparklines and the
//! most recent job for the current service.

use owo_colors::OwoColorize;

use crate::client::{ApiClient, PrometheusData, PrometheusResponse};
use crate::config::PartiriConfig;
use crate::error::Result;
use crate::output::{colored_job_status, ctx, format_datetime, print_result, sparkline};

fn extract_points(data: &PrometheusData) -> Vec<(f64, f64)> {
    data.result
        .first()
        .map(|r| {
            r.values
                .iter()
                .filter_map(|(t, v)| v.parse::<f64>().ok().map(|val| (*t, val)))
                .collect()
        })
        .unwrap_or_default()
}

fn format_span(seconds: f64) -> String {
    let (value, unit) = if seconds < 60.0 {
        (seconds.round(), "second")
    } else if seconds < 3600.0 {
        ((seconds / 60.0).round(), "minute")
    } else if seconds < 86400.0 {
        ((seconds / 3600.0).round(), "hour")
    } else {
        ((seconds / 86400.0).round(), "day")
    };
    let n = value as i64;
    if n == 1 {
        format!("1 {}", unit)
    } else {
        format!("{} {}s", n, unit)
    }
}

fn format_bytes_rate(bytes: f64) -> String {
    if bytes >= 1_048_576.0 {
        format!("{:.1} MB/s", bytes / 1_048_576.0)
    } else if bytes >= 1024.0 {
        format!("{:.0} KB/s", bytes / 1024.0)
    } else {
        format!("{:.0} B/s", bytes)
    }
}

fn print_sparkline(label: &str, resp: &PrometheusResponse, fmt: fn(f64) -> String) {
    print!("  {}  ", label.bold());
    let points = extract_points(&resp.data);
    if points.is_empty() {
        println!("{}", "no data".dimmed());
    } else {
        let last = points.last().unwrap().1;
        let vals: Vec<f64> = points
            .into_iter()
            .rev()
            .take(30)
            .rev()
            .map(|(_, v)| v)
            .collect();
        println!("{}  {}", sparkline(&vals).cyan(), fmt(last).dimmed());
    }
}

fn window_span(resp: &PrometheusResponse) -> Option<String> {
    let points = extract_points(&resp.data);
    let trimmed: Vec<(f64, f64)> = points.into_iter().rev().take(30).rev().collect();
    match (trimmed.first(), trimmed.last()) {
        (Some((t0, _)), Some((t1, _))) if t1 > t0 => Some(format_span(t1 - t0)),
        _ => None,
    }
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1_073_741_824.0 {
        format!("{:.1} GB", bytes / 1_073_741_824.0)
    } else if bytes >= 1_048_576.0 {
        format!("{:.0} MB", bytes / 1_048_576.0)
    } else if bytes >= 1024.0 {
        format!("{:.0} KB", bytes / 1024.0)
    } else {
        format!("{:.0} B", bytes)
    }
}

/// Entry point for `partiri service metrics`. Fetches the service, its CPU/RAM/
/// network metrics, and its jobs; renders sparklines in human mode or raw metric
/// points in JSON mode.
pub fn run(client: &ApiClient, config: &PartiriConfig) -> Result<()> {
    let id = config.id_or_err()?;

    let service = client.read_service(id)?;
    let deploy_tag = config.deploy_tag.as_deref();
    let cpu_resp = client.read_metrics_cpu(id, deploy_tag);
    let mem_resp = client.read_metrics_memory(id, deploy_tag);
    let net_resp = client.read_metrics_network(id, deploy_tag);
    let jobs_resp = client.list_service_jobs(id);

    if ctx().json {
        // Emit raw points for an agent to chart however it wants. Sparklines and
        // colored timestamps are explicitly human-only output.
        let cpu_points = cpu_resp
            .as_ref()
            .ok()
            .map(|r| extract_points(&r.data))
            .unwrap_or_default();
        let mem_points = mem_resp
            .as_ref()
            .ok()
            .map(|r| extract_points(&r.data))
            .unwrap_or_default();
        let net_down = net_resp
            .as_ref()
            .ok()
            .map(|r| extract_points(&r.download.data))
            .unwrap_or_default();
        let net_up = net_resp
            .as_ref()
            .ok()
            .map(|r| extract_points(&r.upload.data))
            .unwrap_or_default();
        let mut jobs_json: Vec<serde_json::Value> = Vec::new();
        if let Ok(mut jobs) = jobs_resp {
            jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            for j in jobs.into_iter().take(10) {
                jobs_json.push(serde_json::json!({
                    "type": j.job_type,
                    "status": j.status,
                    "deploy_ref": j.deploy_ref,
                    "created_at": j.created_at,
                }));
            }
        }
        print_result(&serde_json::json!({
            "service": {
                "id": service.id,
                "name": service.name,
                "deploy_type": service.deploy_type,
                "runtime": service.runtime,
                "deploy_tag": service.deploy_tag,
                "external_sd_url": service.external_sd_url,
            },
            "metrics": {
                "cpu": cpu_points,
                "memory": mem_points,
                "network_download": net_down,
                "network_upload": net_up,
            },
            "jobs": jobs_json,
        }));
        return Ok(());
    }

    println!(
        "\n  {} {}\n",
        service.name.bold(),
        format!("({})", service.deploy_type).dimmed(),
    );

    let span = cpu_resp
        .as_ref()
        .ok()
        .and_then(window_span)
        .or_else(|| mem_resp.as_ref().ok().and_then(window_span))
        .or_else(|| {
            net_resp
                .as_ref()
                .ok()
                .and_then(|r| window_span(&r.download))
        });
    if let Some(s) = span {
        println!("  {}\n", format!("Last {} of activity", s).dimmed());
    }

    match cpu_resp {
        Ok(r) => print_sparkline("CPU", &r, |v| format!("{:.3} cores", v)),
        Err(_) => println!("  {}  {}", "CPU".bold(), "unavailable".dimmed()),
    }
    match mem_resp {
        Ok(r) => print_sparkline("RAM", &r, format_bytes),
        Err(_) => println!("  {}  {}", "RAM".bold(), "unavailable".dimmed()),
    }
    match net_resp {
        Ok(r) => {
            print_sparkline("NET↓", &r.download, format_bytes_rate);
            print_sparkline("NET↑", &r.upload, format_bytes_rate);
        }
        Err(_) => println!("  {}  {}", "NET".bold(), "unavailable".dimmed()),
    }

    println!();
    println!("  {}", "Last job".bold());
    match jobs_resp {
        Ok(jobs) => match jobs.first() {
            Some(job) => {
                let ts = job
                    .created_at
                    .as_deref()
                    .map(format_datetime)
                    .unwrap_or_else(|| "—".to_string());
                let ref_str = job
                    .deploy_ref
                    .as_deref()
                    .map(|r| format!(" {}", r.get(..7).unwrap_or(r).dimmed()))
                    .unwrap_or_default();
                println!(
                    "  {}  {}{}  {}",
                    ts.dimmed(),
                    job.job_type.bold(),
                    ref_str,
                    colored_job_status(&job.status),
                );
            }
            None => println!("  {}", "No jobs found.".dimmed()),
        },
        Err(_) => println!("  {}", "unavailable".dimmed()),
    }
    println!();

    Ok(())
}
