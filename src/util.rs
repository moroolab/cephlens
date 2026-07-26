use std::thread;

use serde_json::Value;

// Per-host SSH probes dominate the wall clock of doctor and snapshot, so they
// run concurrently. The cap keeps a large profile from opening one ssh process
// per host all at once.
pub(crate) const MAX_PARALLEL_HOSTS: usize = 8;

/// Applies `f` to every item with at most `limit` workers in flight and returns
/// the results in input order. `None` marks an item whose worker panicked.
///
/// Items run in batches of `limit` and a batch waits for its slowest member, so
/// a profile larger than `limit` pays one stalled host per batch. Profiles that
/// size stay rare, and the batching keeps this to a plain scoped-thread loop.
pub(crate) fn map_parallel<T, R, F>(items: &[T], limit: usize, f: F) -> Vec<Option<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    // Bind a shared reference so each worker copies the reference instead of
    // trying to move the closure itself.
    let f = &f;
    let mut results = Vec::with_capacity(items.len());
    for batch in items.chunks(limit.max(1)) {
        thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|item| scope.spawn(move || f(item)))
                .collect::<Vec<_>>();
            results.extend(handles.into_iter().map(|handle| handle.join().ok()));
        });
    }
    results
}

pub(crate) fn shell_quote(input: &str) -> String {
    let mut out = String::from("'");
    for ch in input.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub(crate) fn remote_pidfile_cleanup_functions() -> &'static str {
    r#"cephlens_process_tree() {
  root_pid=$1
  printf '%s\n' "$root_pid"
  for child_pid in $(pgrep -P "$root_pid" 2>/dev/null || true); do
    cephlens_process_tree "$child_pid"
  done
}
cephlens_pid_start_time() {
  sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | awk '{print $20}'
}
cephlens_cleanup_pidfile() {
  cleanup_pidfile=$1
  process_marker=$2
  companion_file=${3:-}
  cleanup_pid=$(cat "$cleanup_pidfile" 2>/dev/null || true)
  case "$cleanup_pid" in
    ''|*[!0-9]*) cleanup_pid="" ;;
  esac
  if [ -n "$cleanup_pid" ] && kill -0 "$cleanup_pid" 2>/dev/null; then
    cleanup_cmdline=$(tr '\000' ' ' < "/proc/$cleanup_pid/cmdline" 2>/dev/null || true)
    case "$cleanup_cmdline" in
      *"$process_marker"*)
        cleanup_pids=$(cephlens_process_tree "$cleanup_pid")
        cleanup_targets=""
        for target_pid in $cleanup_pids; do
          target_start=$(cephlens_pid_start_time "$target_pid")
          if [ -n "$target_start" ]; then
            cleanup_targets="$cleanup_targets $target_pid:$target_start"
          fi
        done
        for target in $cleanup_targets; do
          target_pid=${target%%:*}
          target_start=${target#*:}
          [ "$(cephlens_pid_start_time "$target_pid")" = "$target_start" ] || continue
          sudo -n kill -TERM "$target_pid" 2>/dev/null || kill -TERM "$target_pid" 2>/dev/null || true
        done
        sleep 1
        for target in $cleanup_targets; do
          target_pid=${target%%:*}
          target_start=${target#*:}
          [ "$(cephlens_pid_start_time "$target_pid")" = "$target_start" ] || continue
          sudo -n kill -KILL "$target_pid" 2>/dev/null || kill -KILL "$target_pid" 2>/dev/null || true
        done
        ;;
    esac
  fi
  rm -f "$cleanup_pidfile" 2>/dev/null || true
  if [ -n "$companion_file" ]; then
    rm -f "$companion_file" 2>/dev/null || true
  fi
}
"#
}

pub(crate) fn short(value: &str, len: usize) -> String {
    value.chars().take(len).collect()
}

pub(crate) fn clamp_top_scroll(scroll: usize, total: usize, visible: usize) -> usize {
    scroll.min(total.saturating_sub(visible.max(1)))
}

pub(crate) fn clamp_bottom_scroll(scroll: usize, total: usize, visible: usize) -> usize {
    scroll.min(total.saturating_sub(visible.max(1)))
}

pub(crate) fn ptr_str(value: &Value, path: &str) -> String {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn ptr_u64(value: &Value, path: &str) -> u64 {
    value
        .pointer(path)
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

pub(crate) fn ptr_i64(value: &Value, path: &str) -> i64 {
    value
        .pointer(path)
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

pub(crate) fn ptr_f64(value: &Value, path: &str) -> f64 {
    value
        .pointer(path)
        .and_then(Value::as_f64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("echo hello"), "'echo hello'");
        assert_eq!(shell_quote("printf 'x'"), "'printf '\\''x'\\'''");
    }

    #[test]
    fn remote_cleanup_revalidates_process_identity_before_each_signal() {
        let functions = remote_pidfile_cleanup_functions();

        assert_eq!(
            functions
                .matches("[ \"$(cephlens_pid_start_time \"$target_pid\")\" = \"$target_start\" ]")
                .count(),
            2
        );
    }

    #[test]
    fn map_parallel_keeps_input_order_across_batches() {
        let items = (0..20).collect::<Vec<u32>>();

        let doubled = map_parallel(&items, 3, |item| item * 2);

        assert_eq!(doubled.len(), items.len());
        assert!(
            doubled
                .iter()
                .enumerate()
                .all(|(index, value)| *value == Some(index as u32 * 2))
        );
    }

    #[test]
    fn map_parallel_reports_a_panicking_worker_as_none() {
        let items = vec![1, 2, 3];

        let results = map_parallel(&items, 2, |item| {
            assert_ne!(*item, 2, "worker for 2 panics on purpose");
            *item
        });

        assert_eq!(results, vec![Some(1), None, Some(3)]);
    }

    #[test]
    fn scroll_clamps_to_available_rows() {
        assert_eq!(clamp_top_scroll(99, 10, 4), 6);
        assert_eq!(clamp_top_scroll(3, 2, 4), 0);
        assert_eq!(clamp_bottom_scroll(99, 10, 4), 6);
        assert_eq!(clamp_bottom_scroll(3, 0, 4), 0);
    }
}
