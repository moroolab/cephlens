use std::{collections::HashMap, process};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;

use crate::{
    config::ResolvedConfig,
    model::{
        ClusterSummary, HealthCheck, NodeSummary, OsdSummary, PgSummary, PoolSummary, Snapshot,
    },
    ssh::ssh_capture,
    stream::node_facts_snippet,
    util::{MAX_PARALLEL_HOSTS, map_parallel, ptr_f64, ptr_i64, ptr_str, ptr_u64, shell_quote},
};

pub(crate) fn collect_snapshot(cfg: &ResolvedConfig) -> Result<Snapshot> {
    let status_out = ssh_capture(&cfg.admin_host, "sudo -n ceph -s --format json")?;
    let tree_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd tree --format json")?;
    let df_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd df --format json")?;
    // Latency is supplementary, so a cluster that refuses this query still gets
    // a snapshot.
    let perf_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd perf --format json").ok();
    let osdmap_out = ssh_capture(&cfg.admin_host, "sudo -n ceph osd dump --format json").ok();
    let crush_out = ssh_capture(
        &cfg.admin_host,
        "sudo -n ceph osd crush rule dump --format json",
    )
    .ok();
    let pgs_out = ssh_capture(&cfg.admin_host, "sudo -n ceph pg dump pgs --format json").ok();

    let status: Value = serde_json::from_str(status_out.trim())
        .with_context(|| "failed to parse ceph status json")?;
    let tree: Value =
        serde_json::from_str(tree_out.trim()).with_context(|| "failed to parse osd tree json")?;
    let df: Value =
        serde_json::from_str(df_out.trim()).with_context(|| "failed to parse osd df json")?;
    let perf = perf_out.and_then(|perf| serde_json::from_str::<Value>(perf.trim()).ok());
    let osdmap = osdmap_out.and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok());
    let crush = crush_out.and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok());
    let pgs = pgs_out.and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok());

    let cluster = parse_cluster_summary(&status, osdmap.as_ref());
    let osds = parse_osds(&tree, &df, perf.as_ref());
    let pools = parse_pools(osdmap.as_ref(), crush.as_ref(), &tree);
    let abnormal_pgs = parse_abnormal_pgs(pgs.as_ref());
    let nodes = map_parallel(&cfg.hosts, MAX_PARALLEL_HOSTS, |host| collect_node(host))
        .into_iter()
        .zip(&cfg.hosts)
        .map(|(node, host)| node.unwrap_or_else(|| node_worker_panicked(host)))
        .collect();

    Ok(Snapshot {
        captured_at: Utc::now(),
        profile: cfg.profile.clone(),
        admin_host: cfg.admin_host.clone(),
        hosts: cfg.hosts.clone(),
        trace_window_secs: cfg.trace_window_secs,
        cluster,
        nodes,
        osds,
        pools,
        abnormal_pgs,
    })
}

pub(crate) fn parse_cluster_summary(status: &Value, osdmap: Option<&Value>) -> ClusterSummary {
    let pg_states = status
        .pointer("/pgmap/pgs_by_state")
        .and_then(Value::as_array)
        .map(|states| {
            states
                .iter()
                .map(|state| {
                    format!(
                        "{} {}",
                        ptr_u64(state, "/count"),
                        ptr_str(state, "/state_name")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    ClusterSummary {
        fsid: ptr_str(status, "/fsid"),
        health: ptr_str(status, "/health/status"),
        quorum: status
            .pointer("/quorum_names")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        mon_count: ptr_u64(status, "/monmap/num_mons"),
        mgr_available: status
            .pointer("/mgrmap/available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mgr_standbys: ptr_u64(status, "/mgrmap/num_standbys"),
        osds_total: ptr_u64(status, "/osdmap/num_osds"),
        osds_up: ptr_u64(status, "/osdmap/num_up_osds"),
        osds_in: ptr_u64(status, "/osdmap/num_in_osds"),
        pools: ptr_u64(status, "/pgmap/num_pools"),
        pgs: ptr_u64(status, "/pgmap/num_pgs"),
        objects: ptr_u64(status, "/pgmap/num_objects"),
        bytes_used: ptr_u64(status, "/pgmap/bytes_used"),
        bytes_total: ptr_u64(status, "/pgmap/bytes_total"),
        read_bytes_sec: ptr_u64(status, "/pgmap/read_bytes_sec"),
        write_bytes_sec: ptr_u64(status, "/pgmap/write_bytes_sec"),
        read_ops_sec: ptr_u64(status, "/pgmap/read_op_per_sec"),
        write_ops_sec: ptr_u64(status, "/pgmap/write_op_per_sec"),
        recovering_bytes_sec: ptr_u64(status, "/pgmap/recovering_bytes_per_sec"),
        pg_states,
        nearfull_ratio: osdmap
            .map(|map| ptr_f64(map, "/nearfull_ratio"))
            .unwrap_or_default(),
        backfillfull_ratio: osdmap
            .map(|map| ptr_f64(map, "/backfillfull_ratio"))
            .unwrap_or_default(),
        full_ratio: osdmap
            .map(|map| ptr_f64(map, "/full_ratio"))
            .unwrap_or_default(),
        health_checks: parse_health_checks(status),
    }
}

fn parse_health_checks(status: &Value) -> Vec<HealthCheck> {
    let Some(checks) = status.pointer("/health/checks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut parsed = checks
        .iter()
        .map(|(code, check)| HealthCheck {
            code: code.clone(),
            severity: ptr_str(check, "/severity"),
            message: ptr_str(check, "/summary/message"),
            details: check
                .pointer("/detail")
                .and_then(Value::as_array)
                .map(|details| {
                    details
                        .iter()
                        .filter_map(|detail| detail.pointer("/message").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    parsed.sort_by(|left, right| left.code.cmp(&right.code));
    parsed
}

pub(crate) fn parse_pools(
    osdmap: Option<&Value>,
    crush: Option<&Value>,
    tree: &Value,
) -> Vec<PoolSummary> {
    let domains_by_rule = crush
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| {
                    let id = rule.pointer("/rule_id")?.as_i64()?;
                    let domain = rule
                        .pointer("/steps")?
                        .as_array()?
                        .iter()
                        .find(|step| ptr_str(step, "/op").starts_with("chooseleaf"))
                        .map(|step| ptr_str(step, "/type"))
                        .filter(|domain| !domain.is_empty())?;
                    Some((id, domain))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let domain_counts = tree
        .pointer("/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes.iter().fold(HashMap::new(), |mut counts, node| {
                let kind = ptr_str(node, "/type");
                if !kind.is_empty() {
                    *counts.entry(kind).or_insert(0_u64) += 1;
                }
                counts
            })
        })
        .unwrap_or_default();

    let mut pools = osdmap
        .and_then(|map| map.pointer("/pools"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|pool| {
                    let crush_rule = ptr_i64(pool, "/crush_rule");
                    let failure_domain = domains_by_rule
                        .get(&crush_rule)
                        .cloned()
                        .unwrap_or_default();
                    PoolSummary {
                        id: ptr_i64(pool, "/pool"),
                        name: ptr_str(pool, "/pool_name"),
                        size: ptr_u64(pool, "/size"),
                        min_size: ptr_u64(pool, "/min_size"),
                        crush_rule,
                        available_domains: domain_counts
                            .get(&failure_domain)
                            .copied()
                            .unwrap_or_default(),
                        failure_domain,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    pools.sort_by_key(|pool| pool.id);
    pools
}

pub(crate) fn parse_abnormal_pgs(pgs: Option<&Value>) -> Vec<PgSummary> {
    let mut parsed = pgs
        .and_then(|dump| dump.pointer("/pg_stats"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|pg| ptr_str(pg, "/state") != "active+clean")
                .map(|pg| PgSummary {
                    id: ptr_str(pg, "/pgid"),
                    state: ptr_str(pg, "/state"),
                    up: parse_i64_array(pg.pointer("/up")),
                    acting: parse_i64_array(pg.pointer("/acting")),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    parsed.sort_by(|left, right| left.id.cmp(&right.id));
    parsed
}

fn parse_i64_array(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Maps OSD id to the latency `ceph osd perf` reports. The payload is optional,
/// so a cluster where the query fails keeps its status, tree, and df data.
pub(crate) fn parse_osd_perf(perf: Option<&Value>) -> HashMap<i64, (u64, u64)> {
    let mut by_osd = HashMap::new();
    let Some(infos) = perf
        .and_then(|perf| perf.pointer("/osdstats/osd_perf_infos"))
        .and_then(Value::as_array)
    else {
        return by_osd;
    };
    for info in infos {
        by_osd.insert(
            ptr_i64(info, "/id"),
            (
                ptr_u64(info, "/perf_stats/commit_latency_ms"),
                ptr_u64(info, "/perf_stats/apply_latency_ms"),
            ),
        );
    }
    by_osd
}

pub(crate) fn parse_osds(tree: &Value, df: &Value, perf: Option<&Value>) -> Vec<OsdSummary> {
    let latency_by_osd = parse_osd_perf(perf);
    let mut host_by_osd = HashMap::new();
    let mut status_by_osd = HashMap::new();

    if let Some(nodes) = tree.pointer("/nodes").and_then(Value::as_array) {
        for node in nodes {
            if ptr_str(node, "/type") == "host" {
                let host = ptr_str(node, "/name");
                if let Some(children) = node.pointer("/children").and_then(Value::as_array) {
                    for child in children {
                        if let Some(id) = child.as_i64() {
                            host_by_osd.insert(id, host.clone());
                        }
                    }
                }
            } else if ptr_str(node, "/type") == "osd" {
                let id = ptr_i64(node, "/id");
                status_by_osd.insert(id, ptr_str(node, "/status"));
            }
        }
    }

    let mut osds = Vec::new();
    if let Some(nodes) = df.pointer("/nodes").and_then(Value::as_array) {
        for node in nodes {
            let id = ptr_i64(node, "/id");
            let (commit_latency_ms, apply_latency_ms) =
                latency_by_osd.get(&id).copied().unwrap_or_default();
            osds.push(OsdSummary {
                id,
                name: ptr_str(node, "/name"),
                host: host_by_osd.get(&id).cloned().unwrap_or_default(),
                status: status_by_osd
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| ptr_str(node, "/status")),
                reweight: ptr_f64(node, "/reweight"),
                utilization: ptr_f64(node, "/utilization"),
                pgs: ptr_u64(node, "/pgs"),
                used_kb: ptr_u64(node, "/kb_used"),
                avail_kb: ptr_u64(node, "/kb_avail"),
                commit_latency_ms,
                apply_latency_ms,
            });
        }
    }
    osds.sort_by_key(|osd| osd.id);
    osds
}

fn collect_node(host: &str) -> NodeSummary {
    let facts = node_facts_snippet();
    let command = format!(
        r#"
{facts}
read _ user1 nice1 system1 idle1 iowait1 irq1 softirq1 steal1 _ _ < /proc/stat
sleep 0.2
read _ user2 nice2 system2 idle2 iowait2 irq2 softirq2 steal2 _ _ < /proc/stat
idle_a=$((idle1 + iowait1))
idle_b=$((idle2 + iowait2))
non_idle_a=$((user1 + nice1 + system1 + irq1 + softirq1 + steal1))
non_idle_b=$((user2 + nice2 + system2 + irq2 + softirq2 + steal2))
total_a=$((idle_a + non_idle_a))
total_b=$((idle_b + non_idle_b))
diff_total=$((total_b - total_a))
diff_idle=$((idle_b - idle_a))
cpu_pct=$(awk -v total="$diff_total" -v idle="$diff_idle" 'BEGIN {{if (total > 0) printf "%.1f", (total-idle)*100/total; else printf "0.0"}}')
printf 'hostname=%s\n' "$hostname"
printf 'sudo=%s\n' "$sudo_state"
printf 'ceph_version=%s\n' "$ceph_version"
printf 'deployment=%s\n' "$deployment"
printf 'ceph_osd_processes=%s\n' "$count"
printf 'osd_ids=%s\n' "$ids"
printf 'osd_io_write_limits=%s\n' "$io_limits"
printf 'cpu_percent=%s\n' "$cpu_pct"
printf 'mem_percent=%s\n' "$mem_pct"
printf 'io_stall_percent=%s\n' "$io_stall"
printf 'cpu_stall_percent=%s\n' "$cpu_stall"
"#,
        facts = facts
    );
    match ssh_capture(host, &command) {
        Ok(output) => {
            let map = parse_key_values(&output);
            NodeSummary {
                host: host.to_owned(),
                hostname: map.get("hostname").cloned().unwrap_or_default(),
                sudo: map.get("sudo").cloned().unwrap_or_default(),
                ceph_version: map.get("ceph_version").cloned().unwrap_or_default(),
                deployment: map.get("deployment").cloned().unwrap_or_default(),
                ceph_osd_processes: map
                    .get("ceph_osd_processes")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                osd_ids: map.get("osd_ids").cloned().unwrap_or_default(),
                osd_io_write_limits: map.get("osd_io_write_limits").cloned().unwrap_or_default(),
                cpu_percent: map
                    .get("cpu_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                mem_percent: map
                    .get("mem_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                io_stall_percent: map
                    .get("io_stall_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                cpu_stall_percent: map
                    .get("cpu_stall_percent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                error: None,
            }
        }
        Err(err) => NodeSummary {
            host: host.to_owned(),
            error: Some(format!("{err:#}")),
            ..NodeSummary::default()
        },
    }
}

fn node_worker_panicked(host: &str) -> NodeSummary {
    NodeSummary {
        host: host.to_owned(),
        error: Some("node collection worker panicked".to_owned()),
        ..NodeSummary::default()
    }
}

pub(crate) fn run_bench(
    host: &str,
    seconds: u64,
    session: Option<&str>,
    keep_pool: bool,
) -> Result<String> {
    let session = session.map(ToOwned::to_owned).unwrap_or_else(|| {
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_string()
    });
    let pool = bench_pool_name(&session, process::id());
    ssh_capture(host, &bench_command(&pool, seconds, keep_pool))
}

fn bench_pool_name(session: &str, pid: u32) -> String {
    format!("cephlens-test-{session}-{pid}")
}

fn bench_command(pool: &str, seconds: u64, keep_pool: bool) -> String {
    let pool = shell_quote(pool);
    let keep_pool = if keep_pool { "yes" } else { "no" };
    format!(
        r#"
set -eu
pool={pool}
keep_pool={keep_pool}
pool_created=no
cleanup() {{
  code=$?
  trap - INT TERM HUP EXIT
  if [ "$pool_created" = yes ]; then
    if ! sudo -n rados -p "$pool" cleanup >/dev/null 2>&1; then
      echo "cephlens bench cleanup failed for pool $pool" >&2
      if [ "$code" -eq 0 ]; then code=1; fi
    fi
    if [ "$keep_pool" = no ] && ! sudo -n ceph osd pool delete "$pool" "$pool" --yes-i-really-really-mean-it >/dev/null 2>&1; then
      echo "cephlens bench pool deletion failed for $pool" >&2
      if [ "$code" -eq 0 ]; then code=1; fi
    fi
  fi
  exit "$code"
}}
trap cleanup INT TERM HUP EXIT
sudo -n ceph osd pool create "$pool" 32 >/dev/null
pool_created=yes
sudo -n ceph osd pool application enable "$pool" rados >/dev/null
echo "cephlens bench pool=$pool"
sudo -n rados -p "$pool" bench {seconds} write -b 4096 -t 4 --no-cleanup
"#
    )
}

pub(crate) fn run_probe(hosts: &[String]) -> String {
    let mut output = String::new();
    for host in hosts {
        let command = r#"
printf '--- %s ---\n' "$(hostname)"
printf 'kernel='; uname -r
printf 'sudo='; if sudo -n true 2>/dev/null; then echo ok; else echo needs_password; fi
printf 'ceph_version='
ceph_version=$(ceph --version 2>/dev/null | head -1)
if [ -n "$ceph_version" ]; then echo "$ceph_version"; else echo missing; fi
printf 'deployment='
micro=$(snap list microceph 2>/dev/null | awk 'NR==2 {print $2" "$4" "$6; found=1}')
if [ -n "$micro" ]; then
  echo "microceph $micro"
elif command -v cephadm >/dev/null 2>&1; then
  echo cephadm
elif [ -d /var/lib/rook ]; then
  echo rook
else
  echo generic
fi
printf 'ceph_osd='; pgrep -af '[c]eph-osd --cluster ceph' || true
printf 'osdtrace='
bin=$(command -v osdtrace 2>/dev/null || true)
if [ -z "$bin" ] && [ -x "$HOME/.cephlens/bin/osdtrace" ]; then
  bin="$HOME/.cephlens/bin/osdtrace"
fi
if [ -n "$bin" ]; then
  "$bin" --version 2>/dev/null | head -1 | awk -v bin="$bin" '{print bin" "$0; found=1} END {if (!found) print bin}'
else
  echo missing
fi
"#;
        match ssh_capture(host, command) {
            Ok(s) => {
                output.push_str(&format!("probe {host}: ok\n{s}\n"));
            }
            Err(err) => {
                output.push_str(&format!("probe {host}: {err:#}\n"));
            }
        }
    }
    output
}

fn parse_key_values(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes taken from ceph 19.2.3 on a microceph cluster.
    #[test]
    fn cluster_summary_names_the_failing_health_checks() {
        let status: Value = serde_json::from_str(
            r#"{"health":{"status":"HEALTH_WARN","checks":{
                 "OSD_NEARFULL":{"severity":"HEALTH_WARN","summary":{"message":"1 nearfull osd(s)"},
                   "detail":[{"message":"osd.2 is near full at 85%"}]},
                 "MON_CLOCK_SKEW":{"severity":"HEALTH_WARN","summary":{"message":"clock skew detected"}}}}}"#,
        )
        .unwrap();

        let cluster = parse_cluster_summary(&status, None);

        let codes = cluster
            .health_checks
            .iter()
            .map(|check| check.code.as_str())
            .collect::<Vec<_>>();
        assert_eq!(codes, vec!["MON_CLOCK_SKEW", "OSD_NEARFULL"]);
        assert_eq!(cluster.health_checks[1].message, "1 nearfull osd(s)");
        assert_eq!(
            cluster.health_checks[1].details,
            vec!["osd.2 is near full at 85%"]
        );
    }

    #[test]
    fn healthy_cluster_reports_no_checks() {
        let status: Value =
            serde_json::from_str(r#"{"health":{"status":"HEALTH_OK","checks":{},"mutes":[]}}"#)
                .unwrap();

        assert!(
            parse_cluster_summary(&status, None)
                .health_checks
                .is_empty()
        );
    }

    #[test]
    fn cluster_summary_reads_recovery_rate_and_capacity_thresholds() {
        let status: Value = serde_json::from_str(
            r#"{"pgmap":{"recovering_bytes_per_sec":1048576},"health":{"status":"HEALTH_OK"}}"#,
        )
        .unwrap();
        let osdmap: Value = serde_json::from_str(
            r#"{"nearfull_ratio":0.75,"backfillfull_ratio":0.8,"full_ratio":0.85}"#,
        )
        .unwrap();

        let cluster = parse_cluster_summary(&status, Some(&osdmap));

        assert_eq!(cluster.recovering_bytes_sec, 1_048_576);
        assert_eq!(cluster.nearfull_ratio, 0.75);
        assert_eq!(cluster.backfillfull_ratio, 0.8);
        assert_eq!(cluster.full_ratio, 0.85);
    }

    #[test]
    fn pools_include_crush_failure_domain_capacity() {
        let osdmap: Value = serde_json::from_str(
            r#"{"pools":[{"pool":2,"pool_name":"testpool","size":4,"min_size":2,"crush_rule":0}]}"#,
        )
        .unwrap();
        let crush: Value = serde_json::from_str(
            r#"[{"rule_id":0,"steps":[{"op":"take","item":-1},{"op":"chooseleaf_firstn","type":"host"},{"op":"emit"}]}]"#,
        )
        .unwrap();
        let tree: Value = serde_json::from_str(
            r#"{"nodes":[{"type":"root","name":"default"},{"type":"host","name":"node-1"},{"type":"host","name":"node-2"},{"type":"host","name":"node-3"}]}"#,
        )
        .unwrap();

        let pools = parse_pools(Some(&osdmap), Some(&crush), &tree);

        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "testpool");
        assert_eq!(pools[0].size, 4);
        assert_eq!(pools[0].failure_domain, "host");
        assert_eq!(pools[0].available_domains, 3);
    }

    #[test]
    fn abnormal_pgs_keep_state_and_osd_mappings() {
        let dump: Value = serde_json::from_str(
            r#"{"pg_stats":[
              {"pgid":"2.0","state":"active+clean","up":[0,1,2],"acting":[0,1,2]},
              {"pgid":"2.1","state":"active+clean+scrubbing+deep","up":[1,2,0],"acting":[1,2,0]},
              {"pgid":"2.2","state":"active+remapped+backfilling","up":[3,1,2],"acting":[0,1,2]}
            ]}"#,
        )
        .unwrap();

        let pgs = parse_abnormal_pgs(Some(&dump));

        assert_eq!(pgs.len(), 2);
        assert_eq!(pgs[0].id, "2.1");
        assert_eq!(pgs[0].acting, vec![1, 2, 0]);
        assert_eq!(pgs[1].up, vec![3, 1, 2]);
    }

    #[test]
    fn osd_latency_is_merged_by_id_and_optional() {
        let tree: Value = serde_json::from_str(r#"{"nodes":[]}"#).unwrap();
        let df: Value =
            serde_json::from_str(r#"{"nodes":[{"id":1,"name":"osd.1"},{"id":2,"name":"osd.2"}]}"#)
                .unwrap();
        let perf: Value = serde_json::from_str(
            r#"{"osdstats":{"osd_perf_infos":[
                 {"id":2,"perf_stats":{"commit_latency_ms":7,"apply_latency_ms":3}}]}}"#,
        )
        .unwrap();

        let merged = parse_osds(&tree, &df, Some(&perf));
        assert_eq!(merged[0].commit_latency_ms, 0, "osd.1 has no perf entry");
        assert_eq!(merged[1].commit_latency_ms, 7);
        assert_eq!(merged[1].apply_latency_ms, 3);

        // A cluster that refuses `ceph osd perf` still gets its OSD rows.
        let without = parse_osds(&tree, &df, None);
        assert_eq!(without.len(), 2);
        assert_eq!(without[1].commit_latency_ms, 0);
    }

    #[test]
    fn bench_command_cleans_up_unique_pool_on_exit() {
        let pool = bench_pool_name("20260716-120000", 42);
        let command = bench_command(&pool, 5, false);

        assert_eq!(pool, "cephlens-test-20260716-120000-42");
        assert!(command.contains("pool='cephlens-test-20260716-120000-42'"));
        assert!(command.contains("trap cleanup INT TERM HUP EXIT"));
        assert!(command.contains("cephlens bench pool=$pool"));
        assert!(command.contains("rados -p \"$pool\" cleanup"));
        assert!(
            command
                .contains("ceph osd pool delete \"$pool\" \"$pool\" --yes-i-really-really-mean-it")
        );
    }

    #[test]
    fn bench_command_can_retain_pool() {
        let command = bench_command("cephlens-test-20260716-120000-42", 5, true);

        assert!(command.contains("keep_pool=yes"));
    }
}
