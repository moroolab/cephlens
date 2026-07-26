//! Builds the OSD to PG to object aggregation behind the flow panel.
//!
//! Both trace sources already carry the mapping. radostrace lines name the
//! object, its placement group, and the acting set, which gives all three
//! levels. osdtrace lines name the OSD and the placement group but not the
//! object, so that source collapses to two levels. Neither needs a extra
//! remote command.

use std::collections::HashMap;

use crate::{
    radostrace::RadosEvent,
    trace::{TraceEvent, normalize_osd_name},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowMetric {
    Ops,
    Latency,
}

impl FlowMetric {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FlowMetric::Ops => "ops",
            FlowMetric::Latency => "latency",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            FlowMetric::Ops => FlowMetric::Latency,
            FlowMetric::Latency => FlowMetric::Ops,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowSort {
    Descending,
    Ascending,
}

impl FlowSort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FlowSort::Descending => "desc",
            FlowSort::Ascending => "asc",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            FlowSort::Descending => FlowSort::Ascending,
            FlowSort::Ascending => FlowSort::Descending,
        }
    }
}

/// Which source the tree was built from. The panel reports it so an empty tree
/// is distinguishable from a tree that simply cannot name objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlowSource {
    /// radostrace: OSD, PG, and object.
    Rados,
    /// osdtrace: OSD and PG only.
    Osd,
    Empty,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FlowStats {
    pub(crate) ops: u64,
    pub(crate) sum_us: u64,
    pub(crate) max_us: u64,
    pub(crate) sum_bytes: u64,
}

impl FlowStats {
    fn record(&mut self, latency_us: u64, size_bytes: u64) {
        self.ops += 1;
        self.sum_us = self.sum_us.saturating_add(latency_us);
        self.max_us = self.max_us.max(latency_us);
        self.sum_bytes = self.sum_bytes.saturating_add(size_bytes);
    }

    fn merge(&mut self, other: &FlowStats) {
        self.ops += other.ops;
        self.sum_us = self.sum_us.saturating_add(other.sum_us);
        self.max_us = self.max_us.max(other.max_us);
        self.sum_bytes = self.sum_bytes.saturating_add(other.sum_bytes);
    }

    pub(crate) fn avg_us(&self) -> u64 {
        self.sum_us.checked_div(self.ops).unwrap_or(0)
    }

    /// Mean declared op size. Beside the latency it separates a heavy request
    /// from one that is slow for the little it asks for. A read reports the
    /// length requested, so a client that always asks for 4MiB reports 4MiB
    /// whatever the object holds.
    pub(crate) fn avg_bytes(&self) -> u64 {
        self.sum_bytes.checked_div(self.ops).unwrap_or(0)
    }

    fn key(&self, metric: FlowMetric) -> u64 {
        match metric {
            FlowMetric::Ops => self.ops,
            FlowMetric::Latency => self.max_us,
        }
    }
}

/// One rendered line. `depth` is 0 for an OSD, 1 for a PG, and 2 for an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlowRow {
    pub(crate) depth: usize,
    pub(crate) last_child: bool,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) stats: FlowStats,
}

#[derive(Clone, Debug)]
pub(crate) struct FlowTree {
    pub(crate) source: FlowSource,
    pub(crate) rows: Vec<FlowRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlowPath {
    pub(crate) osd: String,
    pub(crate) host: String,
    pub(crate) pg: String,
    pub(crate) acting: String,
    pub(crate) object: Option<String>,
    pub(crate) stats: FlowStats,
}

pub(crate) fn flow_paths(tree: &FlowTree) -> Vec<FlowPath> {
    let mut paths = Vec::new();
    let mut osd: Option<(String, String)> = None;
    let mut pg: Option<(String, String)> = None;

    for row in &tree.rows {
        match row.depth {
            0 => {
                osd = Some((row.label.clone(), row.detail.clone()));
                pg = None;
            }
            1 => {
                pg = Some((row.label.clone(), row.detail.clone()));
                if tree.source == FlowSource::Osd
                    && let Some((osd, host)) = &osd
                {
                    paths.push(FlowPath {
                        osd: osd.clone(),
                        host: host.clone(),
                        pg: row.label.clone(),
                        acting: row.detail.clone(),
                        object: None,
                        stats: row.stats.clone(),
                    });
                }
            }
            _ => {
                if let (Some((osd, host)), Some((pg, acting))) = (&osd, &pg) {
                    paths.push(FlowPath {
                        osd: osd.clone(),
                        host: host.clone(),
                        pg: pg.clone(),
                        acting: acting.clone(),
                        object: Some(row.label.clone()),
                        stats: row.stats.clone(),
                    });
                }
            }
        }
    }

    paths
}

#[derive(Default)]
struct Branch {
    detail: String,
    stats: FlowStats,
    children: HashMap<String, Branch>,
}

impl Branch {
    fn child(&mut self, key: &str) -> &mut Branch {
        self.children.entry(key.to_owned()).or_default()
    }
}

/// Builds the tree, preferring radostrace because it can name objects. Falls
/// back to osdtrace when no RADOS client op has been seen.
pub(crate) fn build_flow_tree(
    rados_events: &[RadosEvent],
    trace_events: &[TraceEvent],
    host_by_osd_id: &HashMap<i64, String>,
    metric: FlowMetric,
    sort: FlowSort,
    max_osds: usize,
    max_children: usize,
) -> FlowTree {
    let (roots, source) = if rados_events.is_empty() {
        (osd_roots(trace_events), FlowSource::Osd)
    } else {
        (rados_roots(rados_events, host_by_osd_id), FlowSource::Rados)
    };
    if roots.children.is_empty() {
        return FlowTree {
            source: FlowSource::Empty,
            rows: Vec::new(),
        };
    }
    FlowTree {
        source,
        rows: flatten(roots, metric, sort, max_osds, max_children),
    }
}

fn rados_roots(events: &[RadosEvent], host_by_osd_id: &HashMap<i64, String>) -> Branch {
    let mut root = Branch::default();
    for event in events {
        let Some(primary) = event.acting.first().copied().filter(|id| *id >= 0) else {
            continue;
        };
        let osd = root.child(&format!("osd.{primary}"));
        osd.detail = host_by_osd_id.get(&primary).cloned().unwrap_or_default();
        osd.stats.record(event.latency_us, event.size_bytes);

        let pg = osd.child(&event.pg);
        pg.detail = format_acting(&event.acting);
        pg.stats.record(event.latency_us, event.size_bytes);

        let object = pg.child(&event.object);
        object.stats.record(event.latency_us, event.size_bytes);
    }
    root
}

fn osd_roots(events: &[TraceEvent]) -> Branch {
    let mut root = Branch::default();
    for event in events {
        if event.osd.is_empty() || event.pg.is_empty() {
            continue;
        }
        let osd = root.child(&normalize_osd_name(&event.osd));
        osd.detail = event.host.clone();
        osd.stats.record(event.op_lat_us, event.size_bytes);

        let pg = osd.child(&event.pg);
        pg.stats.record(event.op_lat_us, event.size_bytes);
    }
    root
}

fn format_acting(acting: &[i64]) -> String {
    let ids = acting
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{ids}]")
}

fn flatten(
    root: Branch,
    metric: FlowMetric,
    sort: FlowSort,
    max_osds: usize,
    max_children: usize,
) -> Vec<FlowRow> {
    let mut rows = Vec::new();
    let osds = ranked(root.children, metric, sort, max_osds);
    let osd_count = osds.len();
    for (index, (label, osd)) in osds.into_iter().enumerate() {
        rows.push(FlowRow {
            depth: 0,
            last_child: index + 1 == osd_count,
            label,
            detail: osd.detail.clone(),
            stats: osd.stats.clone(),
        });
        let pgs = ranked(osd.children, metric, sort, max_children);
        let pg_count = pgs.len();
        for (pg_index, (pg_label, pg)) in pgs.into_iter().enumerate() {
            rows.push(FlowRow {
                depth: 1,
                last_child: pg_index + 1 == pg_count,
                label: pg_label,
                detail: pg.detail.clone(),
                stats: pg.stats.clone(),
            });
            let objects = ranked(pg.children, metric, sort, max_children);
            let object_count = objects.len();
            for (object_index, (object_label, object)) in objects.into_iter().enumerate() {
                rows.push(FlowRow {
                    depth: 2,
                    last_child: object_index + 1 == object_count,
                    label: object_label,
                    detail: String::new(),
                    stats: object.stats,
                });
            }
        }
    }
    rows
}

/// Orders one level by the active metric and keeps the top `limit`. Ties fall
/// back to the label so the panel does not reshuffle between ticks.
fn ranked(
    children: HashMap<String, Branch>,
    metric: FlowMetric,
    sort: FlowSort,
    limit: usize,
) -> Vec<(String, Branch)> {
    let mut entries = children.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left_label, left), (right_label, right)| {
        let ordering = left.stats.key(metric).cmp(&right.stats.key(metric));
        let ordering = match sort {
            FlowSort::Descending => ordering.reverse(),
            FlowSort::Ascending => ordering,
        };
        ordering.then_with(|| left_label.cmp(right_label))
    });
    entries.truncate(limit);
    entries
}

/// Totals every branch so the panel can show what the tree covers.
pub(crate) fn flow_totals(rows: &[FlowRow]) -> FlowStats {
    let mut totals = FlowStats::default();
    for row in rows.iter().filter(|row| row.depth == 0) {
        totals.merge(&row.stats);
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radostrace::parse_rados_event;

    fn rados(line: &str) -> RadosEvent {
        parse_rados_event(line).expect("fixture parses")
    }

    fn hosts() -> HashMap<i64, String> {
        HashMap::from([(1, "node-a".to_owned()), (4, "node-b".to_owned())])
    }

    fn osd_event(host: &str, osd: &str, pg: &str, op_lat_us: u64) -> TraceEvent {
        TraceEvent {
            host: host.to_owned(),
            osd: osd.to_owned(),
            pg: pg.to_owned(),
            op: "op_w".to_owned(),
            size_bytes: 4096,
            op_lat_us,
            throttle_lat_us: 0,
            recv_lat_us: 0,
            dispatch_lat_us: 0,
            queue_lat_us: 0,
            bluestore_lat_us: 0,
            kv_commit_us: 0,
            raw: String::new(),
        }
    }

    #[test]
    fn rados_events_build_all_three_levels() {
        let events = vec![
            rados("1 1 1 2 1f [4,2,3] W 4096 900 obj-a [write][0,4096]"),
            rados("1 1 2 2 1f [4,2,3] W 4096 100 obj-b [write][0,4096]"),
            rados("1 1 3 2 0a [1,2,3] R 4096 50 obj-c [read][0,4096]"),
        ];

        let tree = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Ops,
            FlowSort::Descending,
            8,
            8,
        );

        assert_eq!(tree.source, FlowSource::Rados);
        let shape = tree
            .rows
            .iter()
            .map(|row| (row.depth, row.label.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                (0, "osd.4"),
                (1, "2.1f"),
                (2, "obj-a"),
                (2, "obj-b"),
                (0, "osd.1"),
                (1, "2.0a"),
                (2, "obj-c"),
            ]
        );
        assert_eq!(tree.rows[0].detail, "node-b");
        assert_eq!(tree.rows[1].detail, "[4,2,3]");
        assert_eq!(tree.rows[0].stats.ops, 2);
        assert_eq!(tree.rows[0].stats.max_us, 900);
        assert_eq!(tree.rows[0].stats.avg_us(), 500);
    }

    #[test]
    fn flow_paths_render_complete_directional_lanes() {
        let events = vec![
            rados("1 1 1 2 1f [4,2,3] W 4096 900 obj-a [write][0,4096]"),
            rados("1 1 2 2 1f [4,2,3] W 4096 100 obj-b [write][0,4096]"),
        ];
        let tree = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Latency,
            FlowSort::Descending,
            8,
            8,
        );

        let paths = flow_paths(&tree);

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].osd, "osd.4");
        assert_eq!(paths[0].host, "node-b");
        assert_eq!(paths[0].pg, "2.1f");
        assert_eq!(paths[0].acting, "[4,2,3]");
        assert_eq!(paths[0].object.as_deref(), Some("obj-a"));
        assert_eq!(paths[0].stats.max_us, 900);
    }

    #[test]
    fn latency_metric_outranks_op_count() {
        let events = vec![
            rados("1 1 1 2 01 [1,2,3] W 4096 10 busy [write][0,4096]"),
            rados("1 1 2 2 01 [1,2,3] W 4096 10 busy [write][0,4096]"),
            rados("1 1 3 2 02 [4,2,3] W 4096 9000 slow [write][0,4096]"),
        ];

        let by_ops = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Ops,
            FlowSort::Descending,
            8,
            8,
        );
        assert_eq!(by_ops.rows[0].label, "osd.1", "two ops beats one");

        let by_latency = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Latency,
            FlowSort::Descending,
            8,
            8,
        );
        assert_eq!(by_latency.rows[0].label, "osd.4", "9000us beats 10us");
    }

    #[test]
    fn ascending_sort_reverses_the_ranking() {
        let events = vec![
            rados("1 1 1 2 01 [1,2,3] W 4096 10 a [write][0,4096]"),
            rados("1 1 2 2 01 [1,2,3] W 4096 10 a [write][0,4096]"),
            rados("1 1 3 2 02 [4,2,3] W 4096 10 b [write][0,4096]"),
        ];

        let tree = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Ops,
            FlowSort::Ascending,
            8,
            8,
        );

        assert_eq!(tree.rows[0].label, "osd.4", "the quieter OSD comes first");
    }

    #[test]
    fn osdtrace_collapses_to_two_levels() {
        let events = vec![
            osd_event("node-a", "1", "2.1f", 400),
            osd_event("node-a", "1", "2.0a", 800),
        ];

        let tree = build_flow_tree(
            &[],
            &events,
            &HashMap::new(),
            FlowMetric::Latency,
            FlowSort::Descending,
            8,
            8,
        );

        assert_eq!(tree.source, FlowSource::Osd);
        assert!(
            tree.rows.iter().all(|row| row.depth < 2),
            "osdtrace lines carry no object name"
        );
        assert_eq!(tree.rows[0].label, "osd.1");
        assert_eq!(tree.rows[1].label, "2.0a", "800us outranks 400us");
    }

    #[test]
    fn rados_wins_when_both_sources_have_data() {
        let rados_events = vec![rados("1 1 1 2 01 [1,2,3] W 4096 10 obj [write][0,4096]")];
        let trace_events = vec![osd_event("node-a", "9", "9.9", 99_999)];

        let tree = build_flow_tree(
            &rados_events,
            &trace_events,
            &hosts(),
            FlowMetric::Latency,
            FlowSort::Descending,
            8,
            8,
        );

        assert_eq!(tree.source, FlowSource::Rados);
        assert_eq!(tree.rows[0].label, "osd.1");
    }

    // A 4MiB op that takes 40ms and a 4KiB op that takes 40ms rank the same by
    // latency, so the panel carries the size that tells them apart.
    #[test]
    fn average_op_size_travels_up_the_tree() {
        let events = vec![
            rados("1 1 1 2 04 [1,2,3] R 4194304 40000 big [read][0,4194304]"),
            rados("1 1 2 2 04 [1,2,3] R 4096 40000 small [read][0,4096]"),
        ];

        let tree = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Ops,
            FlowSort::Descending,
            8,
            8,
        );

        let by_label = |label: &str| {
            tree.rows
                .iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("{label} should be in the tree"))
        };
        assert_eq!(by_label("big").stats.avg_bytes(), 4_194_304);
        assert_eq!(by_label("small").stats.avg_bytes(), 4096);
        assert_eq!(
            by_label("2.04").stats.avg_bytes(),
            2_099_200,
            "the PG reports the mean of both ops"
        );
    }

    #[test]
    fn no_events_report_an_empty_tree() {
        let tree = build_flow_tree(
            &[],
            &[],
            &HashMap::new(),
            FlowMetric::Ops,
            FlowSort::Descending,
            8,
            8,
        );

        assert_eq!(tree.source, FlowSource::Empty);
        assert!(tree.rows.is_empty());
    }

    #[test]
    fn limits_apply_per_level() {
        let events = (0..12)
            .map(|index| {
                rados(&format!(
                    "1 1 {index} 2 0{index} [1,2,3] W 4096 10 obj{index} [write][0,4096]"
                ))
            })
            .collect::<Vec<_>>();

        let tree = build_flow_tree(
            &events,
            &[],
            &hosts(),
            FlowMetric::Ops,
            FlowSort::Descending,
            8,
            3,
        );

        let pgs = tree.rows.iter().filter(|row| row.depth == 1).count();
        assert_eq!(pgs, 3, "the per level cap keeps the panel bounded");
    }
}
