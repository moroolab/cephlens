use std::collections::BTreeMap;

use chrono::Utc;

// radostrace (cephtrace librados-client tracer) output, e.g.:
//   15915  89499  8  2  2  [4,2,3]  W  4096  9023  bench..._object7 [set-alloc-hint write][0, 4096]
// columns: pid client tid pool pg acting WR size latency(us) object[ops]
// The MVP aggregates by pool; other columns are parsed away for now.

#[derive(Clone, Debug)]
pub(crate) struct RadosEvent {
    pub(crate) observed_at: i64,
    pub(crate) pool: String,
    /// Full `pool.seq` placement group id built from the pool and pg columns.
    pub(crate) pg: String,
    /// Acting set for the PG, primary first.
    pub(crate) acting: Vec<i64>,
    pub(crate) object: String,
    /// Size the op declared, in bytes. A write reports the data it writes. A
    /// read reports the length it asked for, which can exceed what the object
    /// holds, so this is the weight of the request rather than bytes moved.
    pub(crate) size_bytes: u64,
    pub(crate) write: bool,
    pub(crate) latency_us: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RadosPoolRow {
    pub(crate) pool: String,
    pub(crate) count: u64,
    pub(crate) avg_us: u64,
    pub(crate) max_us: u64,
    pub(crate) writes: u64,
    pub(crate) reads: u64,
}

pub(crate) fn parse_rados_event(line: &str) -> Option<RadosEvent> {
    parse_rados_event_at(line, Utc::now().timestamp())
}

pub(crate) fn parse_rados_event_at(line: &str, observed_at: i64) -> Option<RadosEvent> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // pid client tid pool pg acting WR size latency + at least one object token
    if tokens.len() < 10 || !tokens[0].chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let write = match tokens[6] {
        "W" => true,
        "R" => false,
        _ => return None,
    };
    Some(RadosEvent {
        observed_at,
        pool: tokens[3].to_owned(),
        pg: format!("{}.{}", tokens[3], tokens[4]),
        acting: parse_acting(tokens[5]),
        object: tokens[9].to_owned(),
        size_bytes: tokens[7].parse().unwrap_or_default(),
        write,
        latency_us: tokens[8].parse::<u64>().ok()?,
    })
}

/// Parses the `[4,2,3]` acting set column. The first entry is the primary. A
/// missing slot is reported as `-1` by Ceph and is kept so the set still lines
/// up with what the cluster reported.
fn parse_acting(token: &str) -> Vec<i64> {
    token
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|entry| entry.trim().parse::<i64>().ok())
        .collect()
}

pub(crate) fn rados_pool_rows(events: &[RadosEvent]) -> Vec<RadosPoolRow> {
    let mut stats: BTreeMap<String, (u64, u64, u64, u64, u64)> = BTreeMap::new();
    for event in events {
        let entry = stats.entry(event.pool.clone()).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(event.latency_us);
        entry.2 = entry.2.max(event.latency_us);
        if event.write {
            entry.3 += 1;
        } else {
            entry.4 += 1;
        }
    }
    let mut rows: Vec<RadosPoolRow> = stats
        .into_iter()
        .map(|(pool, (count, sum, max, writes, reads))| RadosPoolRow {
            pool,
            count,
            avg_us: sum.checked_div(count).unwrap_or(0),
            max_us: max,
            writes,
            reads,
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| right.max_us.cmp(&left.max_us))
            .then_with(|| left.pool.cmp(&right.pool))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_op_lines() {
        let write = parse_rados_event(
            "   15915   89499       8     2   2         [4,2,3]   W     4096     9023     bench_object7 [set-alloc-hint write][0, 4096]",
        )
        .expect("write line parses");
        assert_eq!(write.pool, "2");
        assert!(write.write);
        assert_eq!(write.latency_us, 9023);
        assert_eq!(write.pg, "2.2");
        assert_eq!(write.acting, vec![4, 2, 3]);
        assert_eq!(write.object, "bench_object7");
        assert_eq!(write.size_bytes, 4096);

        let read = parse_rados_event(
            "   4210   771   3   5   1e   [1,2,3]   R   4096   412   obj [read][0, 4096]",
        )
        .expect("read line parses");
        assert_eq!(read.pool, "5");
        assert!(!read.write);
        assert_eq!(read.latency_us, 412);
        assert_eq!(read.pg, "5.1e");
        assert_eq!(read.object, "obj", "reads name the object too");
        assert_eq!(read.size_bytes, 4096);
    }

    #[test]
    fn ignores_setup_and_header_lines() {
        assert!(parse_rados_event("Found library librados.so.2 at: /snap/...").is_none());
        assert!(
            parse_rados_event(
                "     pid  client  tid  pool  pg  acting  WR  size  latency  object[ops]"
            )
            .is_none()
        );
        assert!(parse_rados_event("fill_map_hprobes: function Objecter::_send_op").is_none());
        assert!(parse_rados_event("").is_none());
    }

    #[test]
    fn aggregates_pool_rows() {
        let events = vec![
            parse_rados_event("1 1 1 2 1 [1,2,3] W 4096 100 o [write][0,4096]").unwrap(),
            parse_rados_event("1 1 2 2 1 [1,2,3] W 4096 300 o [write][0,4096]").unwrap(),
            parse_rados_event("1 1 3 5 1 [1,2,3] R 4096 50 o [read][0,4096]").unwrap(),
        ];
        let rows = rados_pool_rows(&events);
        assert_eq!(rows[0].pool, "2");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].avg_us, 200);
        assert_eq!(rows[0].max_us, 300);
        assert_eq!(rows[0].writes, 2);
        assert_eq!(rows[0].reads, 0);
        assert_eq!(rows[1].pool, "5");
        assert_eq!(rows[1].reads, 1);
    }
}
