use crate::{
    config::ResolvedConfig,
    runner::probe_trace_host,
    ssh::ssh_output,
    trace::tracer_probe_command,
    util::{MAX_PARALLEL_HOSTS, map_parallel, short},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CheckLevel {
    Ok,
    Warn,
    Bad,
}

struct DoctorCheck {
    level: CheckLevel,
    scope: String,
    detail: String,
}

pub(crate) struct DoctorReport {
    pub(crate) text: String,
    pub(crate) has_bad: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostRole {
    Admin,
    Osd,
    Client,
}

/// One host's worth of doctor checks. Every job talks to a single host, so the
/// jobs run concurrently while each host still sees one SSH connection at a time.
#[derive(Debug)]
struct HostJob<'a> {
    host: &'a str,
    role: HostRole,
}

impl HostJob<'_> {
    fn run(&self) -> Vec<DoctorCheck> {
        match self.role {
            HostRole::Admin => admin_checks(self.host),
            HostRole::Osd => osd_host_checks(self.host),
            HostRole::Client => client_host_checks(self.host),
        }
    }
}

fn host_jobs(cfg: &ResolvedConfig) -> (Vec<HostJob<'_>>, usize) {
    let mut jobs = vec![HostJob {
        host: &cfg.admin_host,
        role: HostRole::Admin,
    }];
    jobs.extend(cfg.hosts.iter().map(|host| HostJob {
        host,
        role: HostRole::Osd,
    }));
    let client_split = jobs.len();
    jobs.extend(cfg.client_hosts.iter().map(|host| HostJob {
        host,
        role: HostRole::Client,
    }));
    (jobs, client_split)
}

pub(crate) fn run_doctor(cfg: &ResolvedConfig) -> DoctorReport {
    let (jobs, client_split) = host_jobs(cfg);
    let results = map_parallel(&jobs, MAX_PARALLEL_HOSTS, HostJob::run);
    let mut results = results
        .into_iter()
        .zip(&jobs)
        .map(|(result, job)| result.unwrap_or_else(|| vec![worker_panicked(job)]))
        .collect::<Vec<_>>();
    let client_results = results.split_off(client_split);

    let mut checks = vec![DoctorCheck {
        level: CheckLevel::Ok,
        scope: "profile".to_owned(),
        detail: format!("{} admin={}", cfg.profile, cfg.admin_host),
    }];
    checks.extend(results.into_iter().flatten());
    if cfg.client_hosts.is_empty() {
        checks.push(DoctorCheck {
            level: CheckLevel::Warn,
            scope: "client_hosts".to_owned(),
            detail: "not configured; kfstrace and radostrace are disabled".to_owned(),
        });
    }
    checks.extend(client_results.into_iter().flatten());

    render_doctor_report(&checks)
}

fn admin_checks(host: &str) -> Vec<DoctorCheck> {
    vec![
        remote_check("admin ssh", host, "hostname >/dev/null", "reachable"),
        remote_check("admin sudo", host, "sudo -n true", "passwordless sudo"),
        remote_check(
            "admin ceph",
            host,
            "sudo -n ceph -s --format json >/dev/null",
            "ceph status",
        ),
        remote_check(
            "admin rados",
            host,
            "sudo -n rados --version >/dev/null",
            "passwordless rados",
        ),
    ]
}

fn osd_host_checks(host: &str) -> Vec<DoctorCheck> {
    vec![
        remote_check(
            &format!("{host} ssh"),
            host,
            "hostname >/dev/null",
            "reachable",
        ),
        remote_check(
            &format!("{host} sudo"),
            host,
            "sudo -n true",
            "passwordless sudo",
        ),
        osdtrace_check(host),
    ]
}

fn client_host_checks(host: &str) -> Vec<DoctorCheck> {
    vec![
        remote_check(
            &format!("{host} ssh"),
            host,
            "hostname >/dev/null",
            "reachable",
        ),
        remote_check(
            &format!("{host} sudo"),
            host,
            "sudo -n true",
            "passwordless sudo",
        ),
        client_tracer_check(host, "kfstrace"),
        client_tracer_check(host, "radostrace"),
    ]
}

fn worker_panicked(job: &HostJob<'_>) -> DoctorCheck {
    DoctorCheck {
        level: CheckLevel::Bad,
        scope: job.host.to_owned(),
        detail: "doctor worker panicked".to_owned(),
    }
}

fn remote_check(scope: &str, host: &str, command: &str, ok_detail: &str) -> DoctorCheck {
    match ssh_output(host, command, None) {
        Ok(output) if output.success => DoctorCheck {
            level: CheckLevel::Ok,
            scope: scope.to_owned(),
            detail: ok_detail.to_owned(),
        },
        Ok(output) => DoctorCheck {
            level: CheckLevel::Bad,
            scope: scope.to_owned(),
            detail: output_detail(&output.stdout, &output.stderr, &output.status),
        },
        Err(err) => DoctorCheck {
            level: CheckLevel::Bad,
            scope: scope.to_owned(),
            detail: short(&format!("{err:#}"), 160),
        },
    }
}

fn osdtrace_check(host: &str) -> DoctorCheck {
    let target = probe_trace_host(host);
    let scope = format!("{host} osdtrace");
    if let Some(error) = target.error {
        DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: short(&error, 160),
        }
    } else if target.installed {
        DoctorCheck {
            level: CheckLevel::Ok,
            scope,
            detail: format!(
                "{} osds={} traceable={}",
                empty_as_dash(&target.binary),
                empty_as_dash(&target.osds),
                empty_as_dash(&target.traceable)
            ),
        }
    } else {
        DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: "missing".to_owned(),
        }
    }
}

fn client_tracer_check(host: &str, tool: &str) -> DoctorCheck {
    let command = tracer_probe_command(tool);
    let scope = format!("{host} {tool}");
    match ssh_output(host, &command, None) {
        Ok(output) if output.stdout.contains("__CEPHLENS_STATUS__ missing") => DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: "missing".to_owned(),
        },
        Ok(output) if output.stdout.contains("__CEPHLENS_ERROR__") => DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: first_marker_detail(&output.stdout),
        },
        Ok(output) if output.success => DoctorCheck {
            level: CheckLevel::Ok,
            scope,
            detail: first_bin_detail(&output.stdout),
        },
        Ok(output) => DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: output_detail(&output.stdout, &output.stderr, &output.status),
        },
        Err(err) => DoctorCheck {
            level: CheckLevel::Bad,
            scope,
            detail: short(&format!("{err:#}"), 160),
        },
    }
}

fn render_doctor_report(checks: &[DoctorCheck]) -> DoctorReport {
    let mut out = String::from("cephlens doctor\n");
    let worst = checks
        .iter()
        .map(|check| check.level)
        .max()
        .unwrap_or(CheckLevel::Ok);
    out.push_str(&format!("status: {}\n\n", level_label(worst)));
    for check in checks {
        out.push_str(&format!(
            "[{}] {:<24} {}\n",
            level_label(check.level),
            check.scope,
            check.detail
        ));
    }
    DoctorReport {
        text: out,
        has_bad: worst == CheckLevel::Bad,
    }
}

fn output_detail(stdout: &str, stderr: &str, status: &str) -> String {
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        status
    };
    short(detail, 160)
}

fn first_marker_detail(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("__CEPHLENS_ERROR__"))
        .map(|line| line.trim().to_owned())
        .unwrap_or_else(|| "check failed".to_owned())
}

fn first_bin_detail(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("__CEPHLENS_BIN__="))
        .map(|line| line.trim().to_owned())
        .unwrap_or_else(|| "installed".to_owned())
}

fn empty_as_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn level_label(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Ok => "ok",
        CheckLevel::Warn => "warn",
        CheckLevel::Bad => "bad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceInstallConfig;

    fn config(hosts: &[&str], client_hosts: &[&str]) -> ResolvedConfig {
        ResolvedConfig {
            profile: "test".to_owned(),
            admin_host: "ceph-admin".to_owned(),
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            client_hosts: client_hosts.iter().map(|host| (*host).to_owned()).collect(),
            refresh_secs: 1,
            trace_auto_start: false,
            trace_window_secs: 10,
            trace_latency_ms: 1,
            trace_ttl_secs: 1800,
            session_keep: 20,
            trace_install: TraceInstallConfig::default(),
        }
    }

    // The jobs run concurrently, so this order is what keeps the rendered
    // report identical to the sequential version: admin, then every OSD host,
    // then every client host.
    #[test]
    fn host_jobs_keep_the_reported_order() {
        let cfg = config(&["node-a", "node-b"], &["client-a"]);

        let (jobs, client_split) = host_jobs(&cfg);

        assert_eq!(
            jobs.iter()
                .map(|job| (job.host, job.role))
                .collect::<Vec<_>>(),
            vec![
                ("ceph-admin", HostRole::Admin),
                ("node-a", HostRole::Osd),
                ("node-b", HostRole::Osd),
                ("client-a", HostRole::Client),
            ]
        );
        assert_eq!(client_split, 3);
    }

    #[test]
    fn host_jobs_without_client_hosts_split_at_the_end() {
        let cfg = config(&["node-a"], &[]);

        let (jobs, client_split) = host_jobs(&cfg);

        assert_eq!(client_split, jobs.len());
    }

    #[test]
    fn renders_worst_status() {
        let report = render_doctor_report(&[
            DoctorCheck {
                level: CheckLevel::Ok,
                scope: "a".to_owned(),
                detail: "ready".to_owned(),
            },
            DoctorCheck {
                level: CheckLevel::Warn,
                scope: "b".to_owned(),
                detail: "missing optional target".to_owned(),
            },
        ]);

        assert!(!report.has_bad);
        assert!(report.text.contains("status: warn"));
        assert!(report.text.contains("[ok]"));
        assert!(report.text.contains("[warn]"));
    }

    #[test]
    fn bad_report_requires_failure_exit() {
        let report = render_doctor_report(&[DoctorCheck {
            level: CheckLevel::Bad,
            scope: "admin rados".to_owned(),
            detail: "permission denied".to_owned(),
        }]);

        assert!(report.has_bad);
        assert!(report.text.contains("status: bad"));
    }
}
