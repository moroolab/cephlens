use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::{
    model::{InflightOp, NodeSummary},
    util::{ptr_f64, ptr_str, ptr_u64},
};

// Each detail query writes its latest successful result atomically. Only the
// foreground status loop emits JSON, so concurrent updates cannot interleave it.
pub(crate) const CLUSTER_STREAM_SETUP: &str = r#"dir="$HOME/.cache/cephlens/status-$$"
mkdir -p "$dir"
for stale in "$HOME"/.cache/cephlens/status-*; do
  [ -d "$stale" ] || continue
  pid=${stale##*status-}
  case "$pid" in ''|*[!0-9]*) continue ;; esac
  [ "$pid" = "$$" ] || kill -0 "$pid" 2>/dev/null || rm -rf "$stale"
done
workers=""
cleanup() {
  trap - INT TERM HUP PIPE EXIT
  for worker in $workers; do kill "$worker" 2>/dev/null || true; done
  wait
  rm -rf "$dir"
}
trap 'exit 0' INT TERM HUP PIPE
trap cleanup EXIT
refresh_detail() {
  name=$1
  interval=$2
  shift 2
  child=""
  trap - EXIT
  trap '[ -z "$child" ] || kill "$child" 2>/dev/null || true; exit 0' INT TERM HUP PIPE
  while true; do
    sudo -n ceph "$@" --format json >"$dir/$name.next" 2>/dev/null &
    child=$!
    if wait "$child" && [ -s "$dir/$name.next" ]; then
      mv "$dir/$name.next" "$dir/$name"
    fi
    sleep "$interval" &
    child=$!
    wait "$child" || exit 0
  done
}"#;

const CLUSTER_STREAM_EMIT: &str = r#"  status=$(tr -d '\n' <"$dir/status")
  tree=$(tr -d '\n' <"$dir/tree")
  df=$(tr -d '\n' <"$dir/df")
  perf=$(tr -d '\n' <"$dir/perf")
  osdmap=$(tr -d '\n' <"$dir/osdmap")
  crush=$(tr -d '\n' <"$dir/crush")
  pgs=$(tr -d '\n' <"$dir/pgs")
  if [ -n "$status" ]; then
    printf '{"type":"status","status":%s,"tree":%s,"df":%s,"perf":%s,"osdmap":%s,"crush":%s,"pgs":%s}\n' "$status" "$tree" "$df" "$perf" "$osdmap" "$crush" "$pgs"
  else
    printf '{"type":"error","message":"ceph status command failed"}\n'
  fi"#;

pub(crate) fn cluster_stream_command(interval_secs: u64) -> String {
    let interval_secs = interval_secs.max(1);
    let map_interval = (interval_secs * 2).max(5);
    let static_interval = interval_secs.max(30);
    format!(
        r#"
{setup}
for name in tree df perf osdmap crush pgs; do printf 'null' >"$dir/$name"; done
refresh_detail tree {static_interval} osd tree &
workers="$workers $!"
refresh_detail df {map_interval} osd df &
workers="$workers $!"
refresh_detail perf {interval_secs} osd perf &
workers="$workers $!"
refresh_detail osdmap {map_interval} osd dump &
workers="$workers $!"
refresh_detail crush {static_interval} osd crush rule dump &
workers="$workers $!"
refresh_detail pgs {map_interval} pg dump pgs &
workers="$workers $!"
while true; do
  sudo -n ceph -s --format json >"$dir/status" 2>/dev/null || : >"$dir/status"
{emit}
  sleep {interval_secs}
done
"#,
        setup = CLUSTER_STREAM_SETUP,
        emit = CLUSTER_STREAM_EMIT,
    )
}

pub(crate) const NODE_STATIC_FACTS_SNIPPET: &str = r#"hostname=$(hostname)
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
fi"#;

pub(crate) const NODE_OSD_FACTS_SNIPPET: &str = r#"# pgrep -c prints 0 and exits 1 when nothing matches, so `|| echo 0` would
# append a second line and break the JSON payload below.
count=$(pgrep -c '[c]eph-osd' 2>/dev/null || true)
count=${count:-0}
ids=$(pgrep -af '[c]eph-osd --cluster ceph' 2>/dev/null | sed -n 's/.*--id[ =]\([0-9][0-9]*\).*/\1/p' | paste -sd, -)
io_limits=""
for pid in $(pgrep -x ceph-osd 2>/dev/null || true); do
  args=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
  osd_id=$(printf '%s\n' "$args" | sed -n 's/.*--id[ =]\([0-9][0-9]*\).*/\1/p')
  cgroup=$(awk -F: '$1 == "0" {print $3; exit}' "/proc/$pid/cgroup" 2>/dev/null)
  unit=${cgroup##*/}
  case "$unit" in
    *.service)
      limit=$(systemctl show "$unit" --property=IOWriteBandwidthMax --value 2>/dev/null |
        tr '\n' ',' | sed 's/,$//')
      ;;
    *) limit="" ;;
  esac
  if [ -n "$osd_id" ] && [ -n "$limit" ]; then
    if [ -n "$io_limits" ]; then io_limits="$io_limits; "; fi
    io_limits="${io_limits}osd.${osd_id}: ${limit}"
  fi
done
inflight_hex=""
if [ -n "$io_limits" ]; then
  for osd_id in $(printf '%s' "$ids" | tr ',' ' '); do
    encoded=$(sudo -n ceph daemon "osd.$osd_id" dump_ops_in_flight 2>/dev/null |
      head -c 32768 | od -An -tx1 | tr -d ' \n')
    if [ -n "$encoded" ]; then
      if [ -n "$inflight_hex" ]; then inflight_hex="$inflight_hex;"; fi
      inflight_hex="${inflight_hex}${osd_id}=${encoded}"
    fi
  done
fi"#;

pub(crate) const NODE_DYNAMIC_FACTS_SNIPPET: &str = r#"pressure_files=""
[ -r /proc/pressure/io ] && pressure_files="$pressure_files /proc/pressure/io"
[ -r /proc/pressure/cpu ] && pressure_files="$pressure_files /proc/pressure/cpu"
metrics=$(awk '
  FILENAME == "/proc/meminfo" && $1 == "MemTotal:" { total=$2 }
  FILENAME == "/proc/meminfo" && $1 == "MemAvailable:" { avail=$2 }
  FILENAME == "/proc/pressure/io" && $1 == "some" { split($2,a,"="); io=a[2] }
  FILENAME == "/proc/pressure/cpu" && $1 == "some" { split($2,a,"="); cpu=a[2] }
  END {
    mem=(total > 0) ? (total-avail)*100/total : 0
    printf "%.1f %.1f %.1f", mem, io, cpu
  }
' /proc/meminfo $pressure_files 2>/dev/null)
set -- $metrics
mem_pct=${1:-0.0}
io_stall=${2:-0.0}
cpu_stall=${3:-0.0}"#;

pub(crate) fn node_facts_snippet() -> String {
    format!("{NODE_STATIC_FACTS_SNIPPET}\n{NODE_OSD_FACTS_SNIPPET}\n{NODE_DYNAMIC_FACTS_SNIPPET}")
}

pub(crate) fn node_stream_command(interval_secs: u64) -> String {
    let interval_secs = interval_secs.max(1);
    let osd_refresh_ticks = 5_u64.div_ceil(interval_secs);
    format!(
        r#"
{static_facts}
{osd_facts}
prev_total=0
prev_idle=0
osd_ticks=0
while true; do
{dynamic_facts}
  read _ user nice system idle iowait irq softirq steal _ _ < /proc/stat
  idle_all=$((idle + iowait))
  non_idle=$((user + nice + system + irq + softirq + steal))
  total=$((idle_all + non_idle))
  if [ "$prev_total" -gt 0 ]; then
    diff_total=$((total - prev_total))
    diff_idle=$((idle_all - prev_idle))
    if [ "$diff_total" -gt 0 ]; then
      cpu_tenths=$(( ((diff_total - diff_idle) * 1000 + diff_total / 2) / diff_total ))
      cpu_pct="$((cpu_tenths / 10)).$((cpu_tenths % 10))"
    else
      cpu_pct=0.0
    fi
  else
    cpu_pct=0.0
  fi
  prev_total=$total
  prev_idle=$idle_all
  printf '{{"type":"node","hostname":"%s","sudo":"%s","ceph_version":"%s","deployment":"%s","ceph_osd_processes":%s,"osd_ids":"%s","osd_io_write_limits":"%s","osd_inflight_hex":"%s","cpu_percent":%s,"mem_percent":%s,"io_stall_percent":%s,"cpu_stall_percent":%s}}\n' "$hostname" "$sudo_state" "$ceph_version" "$deployment" "$count" "$ids" "$io_limits" "$inflight_hex" "$cpu_pct" "$mem_pct" "$io_stall" "$cpu_stall"
  sleep {interval_secs}
  osd_ticks=$((osd_ticks + 1))
  if [ "$osd_ticks" -ge {osd_refresh_ticks} ]; then
{osd_facts}
    osd_ticks=0
  fi
done
"#,
        static_facts = NODE_STATIC_FACTS_SNIPPET,
        osd_facts = NODE_OSD_FACTS_SNIPPET,
        dynamic_facts = NODE_DYNAMIC_FACTS_SNIPPET,
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
        osd_io_write_limits: ptr_str(&value, "/osd_io_write_limits"),
        osd_inflight_ops: parse_inflight_ops(&ptr_str(&value, "/osd_inflight_hex")),
        cpu_percent: ptr_f64(&value, "/cpu_percent"),
        mem_percent: ptr_f64(&value, "/mem_percent"),
        io_stall_percent: ptr_f64(&value, "/io_stall_percent"),
        cpu_stall_percent: ptr_f64(&value, "/cpu_stall_percent"),
        error: None,
    })
}

pub(crate) fn parse_inflight_ops(encoded: &str) -> Vec<InflightOp> {
    let mut parsed = encoded
        .split(';')
        .filter_map(|entry| {
            let (osd, payload) = entry.split_once('=')?;
            let raw = decode_hex(payload)?;
            let value: Value = serde_json::from_str(&raw).ok()?;
            Some(
                value
                    .pointer("/ops_in_flight/ops")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|op| InflightOp {
                        osd: format!("osd.{osd}"),
                        age_seconds: ptr_f64(op, "/age"),
                        flag_point: ptr_str(op, "/type_data/flag_point"),
                        description: ptr_str(op, "/description"),
                    })
                    .filter(|op| !op.description.is_empty())
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect::<Vec<_>>();
    parsed.sort_by(|left, right| right.age_seconds.total_cmp(&left.age_seconds));
    parsed.truncate(8);
    parsed
}

fn decode_hex(value: &str) -> Option<String> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let bytes = pairs
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
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

        let script = format!("{}\nprintf '%s' \"$count\"", node_facts_snippet());
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

    #[cfg(unix)]
    const QUERY_SECS: f64 = 0.4;

    #[cfg(unix)]
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
                format!(
                    r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/queries"
if [ "$1" = -s ]; then sleep 0.05; else sleep "${{DETAIL_QUERY_SECS:-{QUERY_SECS}}}"; fi
echo '{{"stub":true}}'
"#
                ),
            ),
        ] {
            let path = bin.join(name);
            fs::write(&path, body).expect("write stub");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }
        (root, bin)
    }

    #[cfg(unix)]
    #[test]
    fn cluster_stream_emits_status_before_slow_details() {
        use std::{
            fs,
            io::{BufRead, BufReader},
            process::{Command, Stdio},
            time::Instant,
        };
        let (root, bin) = stub_admin_host("fast-status");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {} }} | head -1", cluster_stream_command(1)))
            .env("PATH", format!("{}:{}", bin.display(), env!("PATH")))
            .env("HOME", &root)
            .env("DETAIL_QUERY_SECS", "2")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let mut payload = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut payload)
            .unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        let status = child.wait().unwrap();
        let _ = fs::remove_dir_all(&root);
        assert!(status.success());
        let value: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["type"], "status");
        assert_eq!(value["status"]["stub"], true);
        assert!(value["tree"].is_null());
        assert!(
            elapsed < QUERY_SECS * 2.0,
            "first status waited {elapsed:.2}s for 2s details"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cluster_stream_refreshes_details_without_repeating_static_queries() {
        use std::{fs, process::Command};
        let (root, bin) = stub_admin_host("detail-refresh");
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {} }} | head -3", cluster_stream_command(1)))
            .env("PATH", format!("{}:{}", bin.display(), env!("PATH")))
            .env("HOME", &root)
            .output()
            .unwrap();
        let queries = fs::read_to_string(root.join("queries")).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert!(output.status.success());
        let payload = String::from_utf8(output.stdout).unwrap();
        let values = payload
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 3);
        assert!(values[0]["tree"].is_null());
        for name in ["tree", "df", "perf", "osdmap", "crush", "pgs"] {
            assert_eq!(values[2][name]["stub"], true, "missing {name}");
        }
        assert_eq!(
            queries
                .lines()
                .filter(|line| line.starts_with("osd tree "))
                .count(),
            1
        );
        assert_eq!(
            queries
                .lines()
                .filter(|line| line.starts_with("osd crush "))
                .count(),
            1
        );
    }

    #[test]
    fn node_stream_collects_static_facts_before_the_tick_loop() {
        let script = node_stream_command(1);
        let loop_start = script.find("while true; do").unwrap();
        let hostname = script.find("hostname=$(hostname)").unwrap();
        let microceph = script.find("snap list microceph").unwrap();

        assert!(hostname < loop_start);
        assert!(microceph < loop_start);
        assert_eq!(script.matches("snap list microceph").count(), 1);
    }

    // Regression: the scratch dir the concurrent tick needs was left behind when
    // the controller closed the stream, because a blocked write kills the remote
    // shell with SIGPIPE and an EXIT-only trap never runs.
    #[cfg(unix)]
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
            r#"{"type":"node","hostname":"ceph-admin","sudo":"ok","ceph_version":"ceph version 19.2.0","deployment":"cephadm","ceph_osd_processes":0,"osd_ids":"","osd_io_write_limits":"","cpu_percent":1.5,"mem_percent":42.0}"#,
        )
        .expect("a mon-only node should parse");

        assert_eq!(summary.ceph_osd_processes, 0);
        assert_eq!(summary.osd_ids, "");
    }

    #[test]
    fn node_stream_payload_keeps_osd_block_write_limit_evidence() {
        let summary = parse_node_stream_payload(
            "node-b",
            r#"{"type":"node","hostname":"node-b","sudo":"ok","ceph_osd_processes":1,"osd_ids":"2","osd_io_write_limits":"osd.2: /dev/vdb 131072","cpu_percent":1.5,"mem_percent":42.0}"#,
        )
        .expect("a write-limited OSD should parse");

        assert_eq!(summary.osd_io_write_limits, "osd.2: /dev/vdb 131072");
    }

    #[test]
    fn inflight_ops_decode_oldest_first_with_object_context() {
        let raw = r#"{"ops_in_flight":{"ops":[{"description":"osd_op(client.1 2.a workshop_object [write 0~4096])","age":12.5,"type_data":{"flag_point":"waiting for subops from 2"}},{"description":"osd_op(client.2 2.b second [read 0~4096])","age":3.0,"type_data":{"flag_point":"queued for pg"}}]}}"#;
        let hex = raw
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        let operations = parse_inflight_ops(&format!("2={hex}"));

        assert_eq!(operations.len(), 2);
        assert_eq!(operations[0].osd, "osd.2");
        assert_eq!(operations[0].age_seconds, 12.5);
        assert_eq!(operations[0].flag_point, "waiting for subops from 2");
        assert!(operations[0].description.contains("workshop_object"));
    }
}
