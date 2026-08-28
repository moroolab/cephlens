use std::collections::HashMap;

use crate::{
    kfstrace::{KfsEvent, kfs_op_rows},
    model::{HealthCheck, NodeSummary, Snapshot},
    radostrace::{RadosEvent, rados_pool_rows},
    trace::{TraceEvent, TraceGraphRow, dominant_component},
    util::short,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsightLevel {
    Ok,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Insight {
    pub(crate) level: InsightLevel,
    pub(crate) text: String,
}

pub(crate) struct DiagnoseInput<'a> {
    pub(crate) snapshot: Option<&'a Snapshot>,
    pub(crate) admin_host: &'a str,
    pub(crate) node_summaries: &'a HashMap<String, NodeSummary>,
    pub(crate) stream_counts: Option<(usize, usize)>,
    pub(crate) trace_window_secs: u64,
    pub(crate) trace_events: &'a [TraceEvent],
    pub(crate) trace_rows: &'a [TraceGraphRow],
    pub(crate) kfs_events: &'a [KfsEvent],
    pub(crate) rados_events: &'a [RadosEvent],
    pub(crate) idle_message: Option<&'a str>,
}

pub(crate) fn diagnose(input: DiagnoseInput<'_>) -> Vec<Insight> {
    let mut insights = Vec::new();

    if let Some(snapshot) = input.snapshot {
        if snapshot.cluster.health != "HEALTH_OK" {
            let level = if snapshot.cluster.health == "HEALTH_WARN" {
                InsightLevel::Warn
            } else {
                InsightLevel::Bad
            };
            insights.push(Insight {
                level,
                text: if snapshot.cluster.health_checks.is_empty() {
                    format!(
                        "cluster health {}; run ceph health detail on {}",
                        snapshot.cluster.health, input.admin_host
                    )
                } else {
                    format!(
                        "cluster health {}: {}",
                        snapshot.cluster.health,
                        health_check_summary(&snapshot.cluster.health_checks)
                    )
                },
            });
        }
        insights.extend(cluster_evidence_insights(snapshot));
        if snapshot.abnormal_pgs.is_empty() && !snapshot.cluster.pg_states.contains("active+clean")
        {
            insights.push(Insight {
                level: InsightLevel::Warn,
                text: format!(
                    "PG state {}; trace latency may include recovery/peering",
                    snapshot.cluster.pg_states
                ),
            });
        }
    } else {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: "waiting for first cluster snapshot".to_owned(),
        });
    }

    if let Some((live_streams, total_streams)) = input.stream_counts
        && total_streams > 0
        && live_streams < total_streams
    {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "ssh streams {live_streams}/{total_streams} live; check hosts marked retry/error"
            ),
        });
    }

    insights.extend(io_limit_insights(input.node_summaries));

    if let Some(error) = input
        .trace_events
        .iter()
        .rev()
        .find(|event| event.op == "error")
    {
        insights.push(Insight {
            level: InsightLevel::Bad,
            text: format!("trace error on {}: {}", error.host, short(&error.raw, 72)),
        });
    }

    let active_rows = input
        .trace_rows
        .iter()
        .filter(|row| row.ops > 0)
        .collect::<Vec<_>>();
    if !active_rows.is_empty() {
        insights.extend(osd_trace_insights(
            input.node_summaries,
            &active_rows,
            input.trace_window_secs,
        ));
    }
    insights.extend(node_pressure_insights(input.node_summaries));
    insights.extend(kfs_insights(input.kfs_events));
    insights.extend(rados_insights(input.rados_events));
    if let Some(cross) = cross_source_insight(&active_rows, input.rados_events) {
        insights.push(cross);
    }
    if active_rows.is_empty()
        && input.kfs_events.is_empty()
        && input.rados_events.is_empty()
        && let Some(message) = input.idle_message
    {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: message.to_owned(),
        });
    }

    insights
}

/// Names the checks behind a HEALTH_WARN or HEALTH_ERR. `ceph -s` already
/// carries them, so the operator does not have to leave the TUI to find out why
/// the cluster is unhealthy. Only the first few fit on one insight line.
fn health_check_summary(checks: &[HealthCheck]) -> String {
    const SHOWN: usize = 3;
    let mut summary = checks
        .iter()
        .take(SHOWN)
        .map(|check| {
            let detail = check
                .details
                .first()
                .map(|detail| format!("; {}", short(detail, 48)))
                .unwrap_or_default();
            format!("{} {}{detail}", check.code, short(&check.message, 60))
        })
        .collect::<Vec<_>>()
        .join("; ");
    if checks.len() > SHOWN {
        summary.push_str(&format!(" (+{} more)", checks.len() - SHOWN));
    }
    summary
}

fn cluster_evidence_insights(snapshot: &Snapshot) -> Vec<Insight> {
    let mut insights = Vec::new();

    for pool in snapshot.pools.iter().filter(|pool| {
        pool.available_domains > 0
            && !pool.failure_domain.is_empty()
            && pool.size > pool.available_domains
    }) {
        insights.push(Insight {
            level: InsightLevel::Bad,
            text: format!(
                "pool {} size {} exceeds {} available {} domains (rule {})",
                pool.name, pool.size, pool.available_domains, pool.failure_domain, pool.crush_rule
            ),
        });
    }

    if snapshot.cluster.full_ratio > 0.0
        && let Some(osd) = snapshot
            .osds
            .iter()
            .max_by(|left, right| left.utilization.total_cmp(&right.utilization))
        && osd.utilization / 100.0 >= snapshot.cluster.full_ratio
    {
        insights.push(Insight {
            level: InsightLevel::Bad,
            text: format!(
                "{} utilization {:.3}% crosses full threshold {:.3}% (near {:.3}%, backfill {:.3}%)",
                osd.name,
                osd.utilization,
                snapshot.cluster.full_ratio * 100.0,
                snapshot.cluster.nearfull_ratio * 100.0,
                snapshot.cluster.backfillfull_ratio * 100.0
            ),
        });
    }

    if let Some(pg) = snapshot
        .abnormal_pgs
        .iter()
        .find(|pg| pg.state.contains("scrubbing+deep"))
        .or_else(|| {
            snapshot
                .abnormal_pgs
                .iter()
                .find(|pg| pg.state.contains("backfill") || pg.state.contains("recover"))
        })
        .or_else(|| snapshot.abnormal_pgs.first())
    {
        let more = snapshot.abnormal_pgs.len().saturating_sub(1);
        let suffix = if more > 0 {
            format!("; +{more} more")
        } else {
            String::new()
        };
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "PG {} {} · up {} · acting {}{}",
                pg.id,
                pg.state,
                format_osd_set(&pg.up),
                format_osd_set(&pg.acting),
                suffix
            ),
        });
    }

    if snapshot.cluster.recovering_bytes_sec > 0 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "recovery/backfill moving {}/s while client operations are observed",
                format_bytes(snapshot.cluster.recovering_bytes_sec)
            ),
        });
    }

    insights
}

fn io_limit_insights(node_summaries: &HashMap<String, NodeSummary>) -> Vec<Insight> {
    let mut limited = node_summaries
        .values()
        .filter(|node| !node.osd_io_write_limits.is_empty())
        .collect::<Vec<_>>();
    limited.sort_by(|left, right| left.host.cmp(&right.host));
    limited
        .into_iter()
        .map(|node| Insight {
            level: InsightLevel::Bad,
            text: format!(
                "{} OSD block write limit: {}",
                node.host,
                short(&node.osd_io_write_limits, 72)
            ),
        })
        .collect()
}

fn format_osd_set(osds: &[i64]) -> String {
    format!(
        "[{}]",
        osds.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn node_pressure_insights(node_summaries: &HashMap<String, NodeSummary>) -> Vec<Insight> {
    let mut stalled = node_summaries
        .values()
        .filter(|node| node.io_stall_percent >= 5.0)
        .collect::<Vec<_>>();
    stalled.sort_by(|left, right| {
        right
            .io_stall_percent
            .total_cmp(&left.io_stall_percent)
            .then_with(|| left.host.cmp(&right.host))
    });
    stalled
        .into_iter()
        .take(2)
        .map(|node| Insight {
            level: if node.io_stall_percent >= 20.0 {
                InsightLevel::Bad
            } else {
                InsightLevel::Warn
            },
            text: format!(
                "{} stalled on IO {:.1}% of the last 10s; storage below the OSD is a suspect",
                node.host, node.io_stall_percent
            ),
        })
        .collect()
}

fn osd_trace_insights(
    node_summaries: &HashMap<String, NodeSummary>,
    active_rows: &[&TraceGraphRow],
    trace_window_secs: u64,
) -> Vec<Insight> {
    let mut insights = Vec::new();
    let total_ops = active_rows.iter().map(|row| row.ops).sum::<u64>();
    let worst = active_rows
        .iter()
        .max_by(|left, right| {
            left.max_us
                .cmp(&right.max_us)
                .then_with(|| left.ops.cmp(&right.ops))
        })
        .copied()
        .expect("active_rows is not empty");
    let worst_level = insight_level_for_latency(worst.max_us);
    insights.push(Insight {
        level: worst_level,
        text: format!(
            "last {}s: {total_ops} ops on {} OSDs; worst {} max {} avg {}",
            trace_window_secs.max(1),
            active_rows.len(),
            worst.osd,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us)
        ),
    });

    if worst.queue_max_us > 0 || worst.recv_max_us > 0 {
        insights.push(Insight {
            level: insight_level_for_latency(worst.queue_max_us.max(worst.recv_max_us)),
            text: format!(
                "compare {} / {}: queue {} · recv {} → {}",
                worst.osd,
                worst.host,
                format_latency_us(worst.queue_max_us),
                format_latency_us(worst.recv_max_us),
                queue_recv_verdict(worst.queue_max_us, worst.recv_max_us)
            ),
        });
    }

    let dominant = dominant_component(worst);
    if dominant.value_us > 0 {
        insights.push(Insight {
            level: insight_level_for_latency(dominant.value_us),
            text: format!(
                "largest observed component {} {}; possible area {}; maxima may be from different ops",
                dominant.name,
                format_latency_us(dominant.value_us),
                dominant.suspect
            ),
        });
    } else if worst.max_us >= 10_000 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: "slow op seen, but parsed queue/store/network components are empty; inspect raw osdtrace"
                .to_owned(),
        });
    }

    if worst.hot_pg != "-" {
        insights.push(Insight {
            level: InsightLevel::Info,
            text: format!(
                "busiest PG on {}: {}; compare acting set if it stays busy",
                worst.osd, worst.hot_pg
            ),
        });
    }

    if let Some(node) = node_for_host(node_summaries, &worst.host) {
        if node.cpu_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Bad,
                text: format!(
                    "{} CPU {}%; queue latency may include OSD worker/scheduler pressure",
                    worst.host,
                    format_percent(node.cpu_percent).trim()
                ),
            });
        } else if node.mem_percent >= 85.0 {
            insights.push(Insight {
                level: InsightLevel::Warn,
                text: format!(
                    "{} memory {}%; check OSD memory pressure before deeper trace",
                    worst.host,
                    format_percent(node.mem_percent).trim()
                ),
            });
        }
    }

    let slow_osds = active_rows
        .iter()
        .filter(|row| row.max_us >= 10_000)
        .count();
    if slow_osds >= 2 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "{slow_osds} OSDs over 10ms; shared network/device/controller pressure is possible"
            ),
        });
    } else if worst.max_us < 10_000 {
        insights.push(Insight {
            level: InsightLevel::Ok,
            text: "no obvious slow OSD in the trace window; max latency is below 10ms".to_owned(),
        });
    }

    insights
}

fn queue_recv_verdict(queue_us: u64, recv_us: u64) -> &'static str {
    const CLEAR_DELAY_US: u64 = 10_000;
    if queue_us >= CLEAR_DELAY_US && queue_us >= recv_us.saturating_mul(2) {
        "queue is at least 2x recv; OSD queue delay likely"
    } else if recv_us >= CLEAR_DELAY_US && recv_us >= queue_us.saturating_mul(2) {
        "recv is at least 2x queue; network receive delay likely"
    } else {
        "no clear queue/recv lead"
    }
}

fn kfs_insights(events: &[KfsEvent]) -> Vec<Insight> {
    let mut insights = Vec::new();
    let rows = kfs_op_rows(events);
    let Some(worst) = rows.iter().max_by_key(|row| row.max_us) else {
        return insights;
    };
    let total: u64 = rows.iter().map(|row| row.count).sum();
    insights.push(Insight {
        level: insight_level_for_latency(worst.max_us),
        text: format!(
            "kfstrace: {total} MDS ops; slowest {} max {} avg {}",
            worst.op,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us)
        ),
    });
    let unsafe_total: u64 = rows.iter().map(|row| row.unsafe_count).sum();
    if unsafe_total > 0 {
        insights.push(Insight {
            level: InsightLevel::Warn,
            text: format!(
                "kfstrace: {unsafe_total} unsafe metadata ops awaiting MDS journal commit"
            ),
        });
    }
    insights
}

fn rados_insights(events: &[RadosEvent]) -> Vec<Insight> {
    let mut insights = Vec::new();
    let rows = rados_pool_rows(events);
    let Some(worst) = rows.iter().max_by_key(|row| row.max_us) else {
        return insights;
    };
    let total: u64 = rows.iter().map(|row| row.count).sum();
    insights.push(Insight {
        level: insight_level_for_latency(worst.max_us),
        text: format!(
            "radostrace: {total} client ops; pool {} max {} avg {} ({}W/{}R)",
            worst.pool,
            format_latency_us(worst.max_us),
            format_latency_us(worst.avg_us),
            worst.writes,
            worst.reads
        ),
    });
    insights
}

fn cross_source_insight(
    osd_active: &[&TraceGraphRow],
    rados_events: &[RadosEvent],
) -> Option<Insight> {
    let rados = rados_pool_rows(rados_events);
    let client_max = rados.iter().map(|row| row.max_us).max().unwrap_or(0);
    let server_max = osd_active.iter().map(|row| row.max_us).max().unwrap_or(0);
    if client_max == 0 || server_max == 0 {
        return None;
    }
    let client = format_latency_us(client_max);
    let server = format_latency_us(server_max);
    Some(Insight {
        level: InsightLevel::Info,
        text: format!(
            "cross-source maxima (not time/PG correlated): rados client {client}, osd server {server}; verify raw traces before attribution"
        ),
    })
}

pub(crate) fn insight_level_for_latency(latency_us: u64) -> InsightLevel {
    if latency_us >= 100_000 {
        InsightLevel::Bad
    } else if latency_us >= 10_000 {
        InsightLevel::Warn
    } else if latency_us > 0 {
        InsightLevel::Ok
    } else {
        InsightLevel::Info
    }
}

pub(crate) fn format_latency_us(value: u64) -> String {
    if value >= 1000 {
        format!("{:.1}ms", value as f64 / 1000.0)
    } else if value == 0 {
        "-".to_owned()
    } else {
        format!("{value}us")
    }
}

fn format_percent(value: f64) -> String {
    format!("{value:>4.1}")
}

fn node_for_host<'a>(
    node_summaries: &'a HashMap<String, NodeSummary>,
    host: &str,
) -> Option<&'a NodeSummary> {
    node_summaries.get(host).or_else(|| {
        node_summaries
            .values()
            .find(|node| node.host == host || node.hostname == host)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{ClusterSummary, OsdSummary, PgSummary, PoolSummary},
        radostrace::parse_rados_event,
    };

    fn snapshot() -> Snapshot {
        Snapshot {
            captured_at: chrono::Utc::now(),
            profile: "test".to_owned(),
            admin_host: "admin".to_owned(),
            hosts: vec!["node-a".to_owned()],
            trace_window_secs: 10,
            cluster: ClusterSummary {
                health: "HEALTH_OK".to_owned(),
                pg_states: "1 active+clean".to_owned(),
                ..ClusterSummary::default()
            },
            nodes: Vec::new(),
            osds: Vec::new(),
            pools: Vec::new(),
            abnormal_pgs: Vec::new(),
        }
    }

    fn row(osd: &str, host: &str, max_us: u64, queue_us: u64) -> TraceGraphRow {
        TraceGraphRow {
            osd: osd.to_owned(),
            host: host.to_owned(),
            ops: 2,
            avg_us: max_us / 2,
            max_us,
            throttle_max_us: 0,
            recv_max_us: 0,
            dispatch_max_us: 0,
            queue_max_us: queue_us,
            store_max_us: 0,
            kv_commit_max_us: 0,
            pg_count: 1,
            hot_pg: "1.a:2".to_owned(),
            points: Vec::new(),
        }
    }

    #[test]
    fn osd_insights_flag_dominant_queue_and_cpu_pressure() {
        let rows = vec![row("osd.1", "node-a", 25_000, 20_000)];
        let mut nodes = HashMap::new();
        nodes.insert(
            "node-a".to_owned(),
            NodeSummary {
                host: "node-a".to_owned(),
                cpu_percent: 90.0,
                ..NodeSummary::default()
            },
        );

        let insights = diagnose(DiagnoseInput {
            snapshot: None,
            admin_host: "admin",
            node_summaries: &nodes,
            stream_counts: None,
            trace_window_secs: 10,
            trace_events: &[],
            trace_rows: &rows,
            kfs_events: &[],
            rados_events: &[],
            idle_message: None,
        });

        assert!(insights.iter().any(|insight| {
            insight
                .text
                .contains("largest observed component queue 20.0ms")
        }));
        assert!(insights.iter().any(|insight| {
            insight
                .text
                .contains("queue is at least 2x recv; OSD queue delay likely")
        }));
        assert!(
            insights
                .iter()
                .any(|insight| insight.text.contains("last 10s"))
        );
        assert!(
            insights
                .iter()
                .any(|insight| insight.text.contains("node-a CPU 90.0%"))
        );
    }

    #[test]
    fn cross_source_maxima_are_not_presented_as_attribution() {
        let rows = [row("osd.1", "node-a", 10_000, 0)];
        let rados = vec![
            parse_rados_event("1 1 1 1 01 [1,2,3] W 4096 30000 obj [write][0,4096]")
                .expect("fixture parses"),
        ];

        let insight = cross_source_insight(&[&rows[0]], &rados).unwrap();

        assert_eq!(insight.level, InsightLevel::Info);
        assert!(insight.text.contains("not time/PG correlated"));
        assert!(!insight.text.contains("gap ="));
    }

    #[test]
    fn queue_and_receive_comparison_names_the_workshop_fault_class() {
        assert_eq!(
            queue_recv_verdict(50_000, 100),
            "queue is at least 2x recv; OSD queue delay likely"
        );
        assert_eq!(
            queue_recv_verdict(100, 50_000),
            "recv is at least 2x queue; network receive delay likely"
        );
        assert_eq!(queue_recv_verdict(9_000, 100), "no clear queue/recv lead");
        assert_eq!(
            queue_recv_verdict(50_000, 30_000),
            "no clear queue/recv lead"
        );
    }

    #[test]
    fn cluster_evidence_connects_pool_pg_capacity_and_recovery_faults() {
        let mut snapshot = snapshot();
        snapshot.pools.push(PoolSummary {
            name: "testpool".to_owned(),
            size: 4,
            crush_rule: 0,
            failure_domain: "host".to_owned(),
            available_domains: 3,
            ..PoolSummary::default()
        });
        snapshot.osds.push(OsdSummary {
            name: "osd.2".to_owned(),
            utilization: 42.0,
            ..OsdSummary::default()
        });
        snapshot.cluster.nearfull_ratio = 0.32;
        snapshot.cluster.backfillfull_ratio = 0.36;
        snapshot.cluster.full_ratio = 0.40;
        snapshot.cluster.recovering_bytes_sec = 2 * 1024 * 1024;
        snapshot.abnormal_pgs.push(PgSummary {
            id: "2.a".to_owned(),
            state: "active+remapped+backfilling".to_owned(),
            up: vec![9, 1, 2],
            acting: vec![0, 1, 2],
        });

        let insights = cluster_evidence_insights(&snapshot);

        assert!(insights.iter().any(|insight| {
            insight
                .text
                .contains("pool testpool size 4 exceeds 3 available host domains")
        }));
        assert!(insights.iter().any(|insight| {
            insight
                .text
                .contains("osd.2 utilization 42.000% crosses full threshold 40.000%")
        }));
        assert!(insights.iter().any(|insight| {
            insight
                .text
                .contains("PG 2.a active+remapped+backfilling · up [9,1,2] · acting [0,1,2]")
        }));
        assert!(
            insights
                .iter()
                .any(|insight| insight.text.contains("2.0 MiB/s"))
        );
    }

    #[test]
    fn deep_scrub_and_block_limit_are_named_directly() {
        let mut snapshot = snapshot();
        snapshot.abnormal_pgs.push(PgSummary {
            id: "2.b".to_owned(),
            state: "active+clean+scrubbing+deep".to_owned(),
            up: vec![0, 1, 2],
            acting: vec![0, 1, 2],
        });
        let cluster = cluster_evidence_insights(&snapshot);
        assert!(
            cluster
                .iter()
                .any(|insight| { insight.text.contains("PG 2.b active+clean+scrubbing+deep") })
        );

        let nodes = HashMap::from([(
            "node-b".to_owned(),
            NodeSummary {
                host: "node-b".to_owned(),
                osd_io_write_limits: "osd.2: /dev/vdb 131072".to_owned(),
                ..NodeSummary::default()
            },
        )]);
        let limits = io_limit_insights(&nodes);
        assert_eq!(limits.len(), 1);
        assert!(limits[0].text.contains("node-b OSD block write limit"));
        assert!(limits[0].text.contains("osd.2"));
    }
}
