use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_TRACE_WINDOW_SECS: u64 = 10;
pub(crate) const LEGACY_TRACE_WINDOW_SECS: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) captured_at: DateTime<Utc>,
    pub(crate) profile: String,
    pub(crate) admin_host: String,
    pub(crate) hosts: Vec<String>,
    #[serde(default = "legacy_trace_window_secs")]
    pub(crate) trace_window_secs: u64,
    pub(crate) cluster: ClusterSummary,
    pub(crate) nodes: Vec<NodeSummary>,
    pub(crate) osds: Vec<OsdSummary>,
    #[serde(default)]
    pub(crate) pools: Vec<PoolSummary>,
    #[serde(default)]
    pub(crate) abnormal_pgs: Vec<PgSummary>,
}

fn legacy_trace_window_secs() -> u64 {
    LEGACY_TRACE_WINDOW_SECS
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ClusterSummary {
    pub(crate) fsid: String,
    pub(crate) health: String,
    pub(crate) quorum: Vec<String>,
    pub(crate) mon_count: u64,
    pub(crate) mgr_available: bool,
    pub(crate) mgr_standbys: u64,
    pub(crate) osds_total: u64,
    pub(crate) osds_up: u64,
    pub(crate) osds_in: u64,
    pub(crate) pools: u64,
    pub(crate) pgs: u64,
    pub(crate) objects: u64,
    pub(crate) bytes_used: u64,
    pub(crate) bytes_total: u64,
    pub(crate) read_bytes_sec: u64,
    pub(crate) write_bytes_sec: u64,
    pub(crate) read_ops_sec: u64,
    pub(crate) write_ops_sec: u64,
    #[serde(default)]
    pub(crate) recovering_bytes_sec: u64,
    pub(crate) pg_states: String,
    #[serde(default)]
    pub(crate) nearfull_ratio: f64,
    #[serde(default)]
    pub(crate) backfillfull_ratio: f64,
    #[serde(default)]
    pub(crate) full_ratio: f64,
    #[serde(default)]
    pub(crate) health_checks: Vec<HealthCheck>,
}

/// One entry of `health.checks` from `ceph -s`. The status payload already
/// carries these, so naming the reason a cluster is unhealthy costs no extra
/// remote command.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct HealthCheck {
    pub(crate) code: String,
    pub(crate) severity: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) details: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct NodeSummary {
    pub(crate) host: String,
    pub(crate) hostname: String,
    pub(crate) sudo: String,
    #[serde(default)]
    pub(crate) ceph_version: String,
    #[serde(default)]
    pub(crate) deployment: String,
    pub(crate) ceph_osd_processes: u64,
    pub(crate) osd_ids: String,
    #[serde(default)]
    pub(crate) osd_io_write_limits: String,
    #[serde(default)]
    pub(crate) cpu_percent: f64,
    #[serde(default)]
    pub(crate) mem_percent: f64,
    /// Share of the last 10s where at least one task was stalled waiting on IO,
    /// from `/proc/pressure/io`. Unlike a device utilization figure this needs
    /// no OSD to block device mapping to be meaningful.
    #[serde(default)]
    pub(crate) io_stall_percent: f64,
    #[serde(default)]
    pub(crate) cpu_stall_percent: f64,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct OsdSummary {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) status: String,
    pub(crate) reweight: f64,
    pub(crate) utilization: f64,
    pub(crate) pgs: u64,
    pub(crate) used_kb: u64,
    pub(crate) avail_kb: u64,
    /// Ceph's own view of OSD latency from `ceph osd perf`. It sits next to the
    /// eBPF numbers so a slow OSD can be attributed to Ceph or to the layer
    /// underneath it.
    #[serde(default)]
    pub(crate) commit_latency_ms: u64,
    #[serde(default)]
    pub(crate) apply_latency_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PoolSummary {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) min_size: u64,
    pub(crate) crush_rule: i64,
    pub(crate) failure_domain: String,
    pub(crate) available_domains: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PgSummary {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) up: Vec<i64>,
    pub(crate) acting: Vec<i64>,
}
