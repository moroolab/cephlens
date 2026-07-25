use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::{
    model::NodeSummary,
    util::{ptr_f64, ptr_str, ptr_u64},
};

// Scratch space for one stream's admin query results. It follows the runner
// convention of living under ~/.cache/cephlens and is removed when the remote
// shell exits, which happens as soon as the controller closes the connection.
pub(crate) const CLUSTER_STREAM_SETUP: &str = r#"dir="$HOME/.cache/cephlens/status-$$"
mkdir -p "$dir"
for stale in "$HOME"/.cache/cephlens/status-*; do
  [ -d "$stale" ] || continue
  pid=${stale##*status-}
  case "$pid" in ''|*[!0-9]*) continue ;; esac
  [ "$pid" = "$$" ] || kill -0 "$pid" 2>/dev/null || rm -rf "$stale"
done
cleanup() { rm -rf "$dir"; }
trap 'cleanup; exit 0' INT TERM HUP PIPE
trap cleanup EXIT"#;

// The three admin queries run at once. In sequence a tick cost their sum, which
// pushed the real refresh period well past the configured interval. Each
// redirect truncates its file before the query runs, so a failed query leaves an
// empty file rather than the previous tick's value.
pub(crate) const CLUSTER_STREAM_TICK: &str = r#"  sudo -n ceph -s --format json >"$dir/status" 2>/dev/null &
  sudo -n ceph osd tree --format json >"$dir/tree" 2>/dev/null &
  sudo -n ceph osd df --format json >"$dir/df" 2>/dev/null &
  wait
  status=$(tr -d '\n' <"$dir/status")
  tree=$(tr -d '\n' <"$dir/tree")
  df=$(tr -d '\n' <"$dir/df")
  if [ -n "$status" ] && [ -n "$tree" ] && [ -n "$df" ]; then
    printf '{"type":"status","status":%s,"tree":%s,"df":%s}\n' "$status" "$tree" "$df"
  else
    printf '{"type":"error","message":"ceph command failed"}\n'
  fi"#;

pub(crate) fn cluster_stream_command(interval_secs: u64) -> String {
    format!(
        r#"
{setup}
while true; do
{tick}
  sleep {interval_secs}
done
"#,
        setup = CLUSTER_STREAM_SETUP,
        tick = CLUSTER_STREAM_TICK,
    )
}

// Shared node facts collection: sets hostname, sudo_state, ceph_version,
// deployment, count, ids, and mem_pct shell variables. Used by both the
// one-shot probe in collect.rs and the streaming loop below.
pub(crate) const NODE_FACTS_SNIPPET: &str = r#"hostname=$(hostname)
if sudo -n true 2>/dev/null; then sudo_state=ok; else sudo_state=needs_password; fi
ceph_version=$(ceph --version 2>/dev/null | head -1)
if [ -z "$ceph_version" ]; then ceph_version=missing; fi
deployment=generic
micro=$(snap list microceph 2>/dev/null | awk 'NR==2 {print $2" "$4" "$6; found=1}')
if [ -n "$micro" ]; then
  deployment="microceph $micro"
elif command -v cephadm >/dev/null 2>&1; then
  deployment=cephadm
elif [ -d /var/lib/rook ]; then
  deployment=rook
fi
# pgrep -c prints 0 and exits 1 when nothing matches, so `|| echo 0` would
# append a second line and break the JSON payload below.
count=$(pgrep -c '[c]eph-osd' 2>/dev/null || true)
count=${count:-0}
ids=$(pgrep -af '[c]eph-osd --cluster ceph' 2>/dev/null | sed -n 's/.*--id \([0-9][0-9]*\).*/\1/p' | paste -sd, -)
mem_pct=$(awk '/MemTotal:/ {total=$2} /MemAvailable:/ {avail=$2} END {if (total > 0) printf "%.1f", (total-avail)*100/total; else printf "0.0"}' /proc/meminfo)"#;

pub(crate) fn node_stream_command(interval_secs: u64) -> String {
    format!(
        r#"
prev_total=0
prev_idle=0
while true; do
{facts}
  read _ user nice system idle iowait irq softirq steal _ _ < /proc/stat
  idle_all=$((idle + iowait))
  non_idle=$((user + nice + system + irq + softirq + steal))
  total=$((idle_all + non_idle))
  if [ "$prev_total" -gt 0 ]; then
    diff_total=$((total - prev_total))
    diff_idle=$((idle_all - prev_idle))
    if [ "$diff_total" -gt 0 ]; then
      cpu_pct=$(awk -v total="$diff_total" -v idle="$diff_idle" 'BEGIN {{ printf "%.1f", (total-idle)*100/total }}')
    else
      cpu_pct=0.0
    fi
  else
    cpu_pct=0.0
  fi
  prev_total=$total
  prev_idle=$idle_all
  printf '{{"type":"node","hostname":"%s","sudo":"%s","ceph_version":"%s","deployment":"%s","ceph_osd_processes":%s,"osd_ids":"%s","cpu_percent":%s,"mem_percent":%s}}\n' "$hostname" "$sudo_state" "$ceph_version" "$deployment" "$count" "$ids" "$cpu_pct" "$mem_pct"
  sleep {interval_secs}
done
"#,
        facts = NODE_FACTS_SNIPPET,
    )
}

pub(crate) fn parse_node_stream_payload(host: &str, payload: &str) -> Result<NodeSummary> {
    let value: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid node stream payload from {host}"))?;
    if value.pointer("/type").and_then(Value::as_str) == Some("error") {
        return Err(anyhow!(
            "{}",
            value
                .pointer("/message")
                .and_then(Value::as_str)
                .unwrap_or("remote node probe failed")
        ));
    }
    Ok(NodeSummary {
        host: host.to_owned(),
        hostname: ptr_str(&value, "/hostname"),
        sudo: ptr_str(&value, "/sudo"),
        ceph_version: ptr_str(&value, "/ceph_version"),
        deployment: ptr_str(&value, "/deployment"),
        ceph_osd_processes: ptr_u64(&value, "/ceph_osd_processes"),
        osd_ids: ptr_str(&value, "/osd_ids"),
        cpu_percent: ptr_f64(&value, "/cpu_percent"),
        mem_percent: ptr_f64(&value, "/mem_percent"),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: `pgrep -c` exits 1 when nothing matches, so a `|| echo 0`
    // fallback emitted a second line and produced `"ceph_osd_processes":0\n0`
    // on every host without ceph-osd processes, such as a mon-only admin node.
    #[cfg(target_os = "linux")]
    #[test]
    fn node_facts_report_osd_count_as_a_bare_integer() {
        use std::process::Command;

        let script = format!("{NODE_FACTS_SNIPPET}\nprintf '%s' \"$count\"");
        let output = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh should be available");
        assert!(
            output.status.success(),
            "node facts snippet failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let count = String::from_utf8_lossy(&output.stdout);
        assert!(
            !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()),
            "ceph_osd_processes must be a bare integer, got {count:?}"
        );
    }

    #[cfg(target_os = "linux")]
    const QUERY_SECS: f64 = 0.4;

    /// Builds a HOME with `sudo` and `ceph` stubs on PATH so the cluster stream
    /// can run without a cluster. Each stubbed query sleeps `QUERY_SECS`.
    #[cfg(target_os = "linux")]
    fn stub_admin_host(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::{fs, os::unix::fs::PermissionsExt};

        let root = std::env::temp_dir().join(format!("cephlens-{label}-{}", std::process::id()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("stub dir");
        for (name, body) in [
            (
                "sudo",
                "#!/bin/sh\n[ \"$1\" = \"-n\" ] && shift\nexec \"$@\"\n".to_owned(),
            ),
            (
                "ceph",
                format!("#!/bin/sh\nsleep {QUERY_SECS}\necho '{{\"stub\":true}}'\n"),
            ),
        ] {
            let path = bin.join(name);
            fs::write(&path, body).expect("write stub");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }
        (root, bin)
    }

    // A sequential tick costs three stubbed sleeps and a concurrent one costs a
    // single sleep. The bound sits between them with room on both sides so
    // ordinary scheduling noise cannot flip the result.
    #[cfg(target_os = "linux")]
    #[test]
    fn cluster_stream_tick_runs_its_queries_concurrently() {
        use std::{fs, process::Command, time::Instant};

        let (root, bin) = stub_admin_host("tick");

        let script = format!("{CLUSTER_STREAM_SETUP}\n{CLUSTER_STREAM_TICK}\n");
        let started = Instant::now();
        let output = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("PATH", format!("{}:{}", bin.display(), env!("PATH")))
            .env("HOME", &root)
            .output()
            .expect("sh should be available");
        let elapsed = started.elapsed().as_secs_f64();
        let payload = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let _ = fs::remove_dir_all(&root);

        let value: Value = serde_json::from_str(&payload).expect("tick emits one JSON object");
        assert_eq!(
            value.pointer("/type").and_then(Value::as_str),
            Some("status")
        );
        assert!(value.pointer("/status").is_some());
        assert!(value.pointer("/tree").is_some());
        assert!(value.pointer("/df").is_some());
        assert!(
            elapsed < QUERY_SECS * 2.0,
            "tick took {elapsed:.2}s; three concurrent {QUERY_SECS}s queries should not approach \
             the {:.2}s sequential cost",
            QUERY_SECS * 3.0
        );
    }

    // Regression: the scratch dir the concurrent tick needs was left behind when
    // the controller closed the stream, because a blocked write kills the remote
    // shell with SIGPIPE and an EXIT-only trap never runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn cluster_stream_removes_its_scratch_dir_when_the_reader_closes() {
        use std::{fs, process::Command};

        let (root, bin) = stub_admin_host("scratch");

        // `head -1` closes the pipe after the first tick, so the loop dies the
        // same way it does when cephlens drops the SSH stream.
        let loop_script = cluster_stream_command(1);
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {loop_script} }} | head -1"))
            .env("PATH", format!("{}:{}", bin.display(), env!("PATH")))
            .env("HOME", &root)
            .output()
            .expect("sh should be available");

        let cache = root.join(".cache").join("cephlens");
        let leftovers = fs::read_dir(&cache)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("status-"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let _ = fs::remove_dir_all(&root);

        assert!(
            !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
            "the stream should emit at least one tick before the reader closes"
        );
        assert!(
            leftovers.is_empty(),
            "stream left scratch dirs behind: {leftovers:?}"
        );
    }

    #[test]
    fn node_stream_payload_parses_a_host_without_osds() {
        let summary = parse_node_stream_payload(
            "ceph-admin",
            r#"{"type":"node","hostname":"ceph-admin","sudo":"ok","ceph_version":"ceph version 19.2.0","deployment":"cephadm","ceph_osd_processes":0,"osd_ids":"","cpu_percent":1.5,"mem_percent":42.0}"#,
        )
        .expect("a mon-only node should parse");

        assert_eq!(summary.ceph_osd_processes, 0);
        assert_eq!(summary.osd_ids, "");
    }
}
