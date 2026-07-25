use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) captured_at: DateTime<Utc>,
    pub(crate) profile: String,
    pub(crate) admin_host: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) cluster: ClusterSummary,
    pub(crate) nodes: Vec<NodeSummary>,
    pub(crate) osds: Vec<OsdSummary>,
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
    pub(crate) pg_states: String,
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
