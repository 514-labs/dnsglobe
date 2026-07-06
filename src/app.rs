use std::collections::HashMap;
use std::time::{Duration, Instant};

use hickory_resolver::proto::rr::RecordType;

use crate::dns::{QueryOutcome, QueryResult};
use crate::resolvers::RESOLVERS;

pub const RECORD_TYPES: &[RecordType] = &[
    RecordType::A,
    RecordType::AAAA,
    RecordType::CNAME,
    RecordType::MX,
    RecordType::NS,
    RecordType::TXT,
    RecordType::SOA,
];

pub const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Table ordering, cycled with Ctrl+S. Sorts are stable, so ties keep the
/// curated resolver-list order — `Location` therefore doubles as "group by
/// location", and `Answer` groups identical answers together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Resolver,
    Location,
    Time,
    Status,
    Answer,
}

impl SortMode {
    pub const fn label(self) -> &'static str {
        match self {
            SortMode::Resolver => "resolver",
            SortMode::Location => "location",
            SortMode::Time => "time",
            SortMode::Status => "status",
            SortMode::Answer => "answer",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            SortMode::Resolver => SortMode::Location,
            SortMode::Location => SortMode::Time,
            SortMode::Time => SortMode::Status,
            SortMode::Status => SortMode::Answer,
            SortMode::Answer => SortMode::Resolver,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RowState {
    Idle,
    Pending,
    Done {
        result: QueryResult,
        elapsed: Duration,
    },
}

#[derive(Debug, Default)]
pub struct Summary {
    pub done: usize,
    pub ok: usize,
    pub no_records: usize,
    pub errors: usize,
    /// Resolvers that gave a usable answer (records or an authoritative
    /// "no records"). Timeouts and refusals say nothing about propagation,
    /// so percentages are computed against this, not the full list.
    pub responding: usize,
    /// Distinct answer *groups*. Answers sharing any record are grouped
    /// together, so round-robin subsets of one pool count as a single group
    /// instead of flagging every resolver as divergent.
    pub groups: usize,
    /// Resolvers in the largest group.
    pub agree: usize,
    /// Per-row flag: true when that resolver's answer is in the largest group.
    pub majority_rows: Vec<bool>,
    /// Union of record values across the largest group.
    pub majority_values: Vec<String>,
}

pub struct App {
    pub domain: String,
    /// Cursor position in `domain`. The input only accepts ASCII
    /// (alphanumerics, `.`, `-`, `_`), so byte index == char index.
    pub cursor: usize,
    pub rtype_idx: usize,
    pub rows: Vec<RowState>,
    pub generation: u64,
    pub spinner_frame: usize,
    pub should_quit: bool,
    pub queried: Option<(String, RecordType)>,
    /// Table scroll offset; clamped against the viewport during draw.
    pub scroll: usize,
    /// Watch mode: re-poll after each round until propagation reaches 100%.
    /// Enabled by starting a query, toggled with Ctrl+R.
    pub auto_refresh: bool,
    /// When the next poll fires, if one is scheduled.
    pub next_poll: Option<Instant>,
    /// Active table ordering, cycled with Ctrl+S.
    pub sort: SortMode,
}

impl App {
    pub fn new(domain: String) -> Self {
        Self {
            cursor: domain.len(),
            domain,
            rtype_idx: 0,
            rows: vec![RowState::Idle; RESOLVERS.len()],
            generation: 0,
            spinner_frame: 0,
            should_quit: false,
            queried: None,
            scroll: 0,
            auto_refresh: false,
            next_poll: None,
            sort: SortMode::default(),
        }
    }

    pub fn record_type(&self) -> RecordType {
        RECORD_TYPES[self.rtype_idx]
    }

    pub fn insert_char(&mut self, c: char) {
        self.domain.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.domain.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.domain.len() {
            self.domain.remove(self.cursor);
        }
    }

    pub fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.domain.len());
    }

    pub fn clear_domain(&mut self) {
        self.domain.clear();
        self.cursor = 0;
    }

    pub fn cycle_record_type(&mut self, forward: bool) {
        let n = RECORD_TYPES.len();
        self.rtype_idx = if forward {
            (self.rtype_idx + 1) % n
        } else {
            (self.rtype_idx + n - 1) % n
        };
    }

    /// Arm a new query round. Returns what to query, or None if the domain
    /// input is empty.
    pub fn begin_query(&mut self) -> Option<(String, RecordType, u64)> {
        let domain = self.domain.trim().trim_end_matches('.').to_string();
        if domain.is_empty() {
            return None;
        }
        self.generation += 1;
        self.rows = vec![RowState::Pending; RESOLVERS.len()];
        self.queried = Some((domain.clone(), self.record_type()));
        Some((domain, self.record_type(), self.generation))
    }

    /// Arm a poll of the last-queried domain/type, ignoring the (possibly
    /// mid-edit) input field.
    pub fn begin_requery(&mut self) -> Option<(String, RecordType, u64)> {
        let (domain, rtype) = self.queried.clone()?;
        self.generation += 1;
        self.rows = vec![RowState::Pending; RESOLVERS.len()];
        Some((domain, rtype, self.generation))
    }

    pub fn apply(&mut self, outcome: QueryOutcome) {
        if outcome.generation != self.generation {
            return; // stale result from a superseded query round
        }
        self.rows[outcome.resolver_index] = RowState::Done {
            result: outcome.result,
            elapsed: outcome.elapsed,
        };
    }

    pub fn in_flight(&self) -> bool {
        self.rows.iter().any(|r| matches!(r, RowState::Pending))
    }

    /// Resolver indices in display order under the active sort.
    pub fn display_order(&self, summary: &Summary) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.rows.len()).collect();
        match self.sort {
            SortMode::Resolver => {}
            SortMode::Location => order.sort_by_key(|&i| RESOLVERS[i].location),
            // Fastest first; rows without a result sink to the bottom.
            SortMode::Time => order.sort_by_key(|&i| match &self.rows[i] {
                RowState::Done { elapsed, .. } => *elapsed,
                _ => Duration::MAX,
            }),
            // Problems first: what's blocking propagation is what you scan
            // for. Mid-flight every answer counts as majority, matching the
            // table's "✓ OK until the round settles" display.
            SortMode::Status => {
                let in_flight = self.in_flight();
                order.sort_by_key(|&i| match &self.rows[i] {
                    RowState::Done { result, .. } => match result {
                        QueryResult::Records { .. } if in_flight || summary.majority_rows[i] => 3,
                        QueryResult::Records { .. } => 0, // differs
                        QueryResult::NoRecords(_) => 1,
                        QueryResult::Error(_) => 2,
                    },
                    RowState::Pending => 4,
                    RowState::Idle => 5,
                });
            }
            SortMode::Answer => order.sort_by(|&a, &b| {
                let values = |i: usize| match &self.rows[i] {
                    RowState::Done {
                        result: QueryResult::Records { values, .. },
                        ..
                    } => Some(values),
                    _ => None,
                };
                // Some < None puts answerless rows last.
                match (values(a), values(b)) {
                    (Some(va), Some(vb)) => va.cmp(vb),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            }),
        }
        order
    }

    pub fn summary(&self) -> Summary {
        let n = self.rows.len();
        let mut summary = Summary {
            majority_rows: vec![false; n],
            ..Default::default()
        };

        // Union-find over rows: two answers belong to the same group when
        // they share at least one record value. This keeps round-robin DNS
        // (each resolver caching a different subset of a pool) in one group.
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }

        let mut first_seen: HashMap<&str, usize> = HashMap::new();
        let mut ok_rows: Vec<usize> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let RowState::Done { result, .. } = row else {
                continue;
            };
            summary.done += 1;
            match result {
                QueryResult::Records { values, .. } => {
                    summary.ok += 1;
                    ok_rows.push(i);
                    for value in values {
                        match first_seen.get(value.as_str()) {
                            Some(&other) => {
                                let a = find(&mut parent, i);
                                let b = find(&mut parent, other);
                                parent[a] = b;
                            }
                            None => {
                                first_seen.insert(value, i);
                            }
                        }
                    }
                }
                QueryResult::NoRecords(_) => summary.no_records += 1,
                QueryResult::Error(_) => summary.errors += 1,
            }
        }
        summary.responding = summary.ok + summary.no_records;

        let mut counts: HashMap<usize, usize> = HashMap::new();
        for &i in &ok_rows {
            let root = find(&mut parent, i);
            *counts.entry(root).or_insert(0) += 1;
        }
        summary.groups = counts.len();

        // Deterministic majority pick: first (in resolver order) among the
        // largest groups.
        let mut majority_root = None;
        let mut best = 0;
        for &i in &ok_rows {
            let root = find(&mut parent, i);
            if counts[&root] > best {
                best = counts[&root];
                majority_root = Some(root);
            }
        }
        if let Some(root) = majority_root {
            summary.agree = counts[&root];
            let mut union: Vec<String> = Vec::new();
            for &i in &ok_rows {
                if find(&mut parent, i) == root {
                    summary.majority_rows[i] = true;
                    if let RowState::Done {
                        result: QueryResult::Records { values, .. },
                        ..
                    } = &self.rows[i]
                    {
                        union.extend(values.iter().cloned());
                    }
                }
            }
            union.sort();
            union.dedup();
            summary.majority_values = union;
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_answers(answers: &[&[&str]]) -> App {
        let mut app = App::new("example.com".into());
        app.rows = answers
            .iter()
            .map(|values| RowState::Done {
                result: QueryResult::Records {
                    values: values.iter().map(|v| v.to_string()).collect(),
                    min_ttl: 60,
                },
                elapsed: Duration::from_millis(10),
            })
            .collect();
        app
    }

    #[test]
    fn round_robin_subsets_form_one_group() {
        // Different 2-IP subsets of one pool, chained by shared members.
        let app = app_with_answers(&[&["a", "b"], &["b", "c"], &["c", "d"], &["a", "d"]]);
        let s = app.summary();
        assert_eq!(s.groups, 1);
        assert_eq!(s.agree, 4);
        assert!(s.majority_rows.iter().all(|&m| m));
        assert_eq!(s.majority_values, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn disjoint_answer_is_flagged_as_minority() {
        let app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        let s = app.summary();
        assert_eq!(s.groups, 2);
        assert_eq!(s.agree, 2);
        assert_eq!(s.majority_rows, vec![true, true, false]);
    }

    #[test]
    fn full_agreement_means_agree_equals_responding() {
        // The watch-mode stop condition: agree == responding.
        let answers = vec![&["x"] as &[&str]; crate::resolvers::RESOLVERS.len()];
        let app = app_with_answers(&answers);
        let s = app.summary();
        assert_eq!(s.responding, crate::resolvers::RESOLVERS.len());
        assert_eq!(s.agree, s.responding);
    }

    #[test]
    fn unreachable_resolvers_do_not_block_full_propagation() {
        // Refused/timed-out resolvers carry no signal: with one error row,
        // the rest agreeing still counts as 100% (agree == responding).
        let mut app = app_with_answers(&vec![&["x"] as &[&str]; RESOLVERS.len() - 1]);
        app.rows.push(RowState::Done {
            result: QueryResult::Error("refused".into()),
            elapsed: Duration::from_secs(3),
        });
        let s = app.summary();
        assert_eq!(s.groups, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(s.responding, RESOLVERS.len() - 1);
        assert_eq!(s.agree, s.responding);
    }

    #[test]
    fn location_sort_groups_locations_and_keeps_curated_order_within() {
        let mut app = app_with_answers(&vec![&["x"] as &[&str]; RESOLVERS.len()]);
        app.sort = SortMode::Location;
        let summary = app.summary();
        let order = app.display_order(&summary);
        let locations: Vec<&str> = order.iter().map(|&i| RESOLVERS[i].location).collect();
        assert!(locations.is_sorted());
        // Stable sort: within one location, curated order is preserved.
        let us: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| RESOLVERS[i].location == "US")
            .collect();
        assert!(us.is_sorted());
    }

    #[test]
    fn status_sort_puts_problems_first_and_time_sort_fastest_first() {
        let mut app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        app.rows.push(RowState::Done {
            result: QueryResult::Error("timeout".into()),
            elapsed: Duration::from_secs(3),
        });
        app.sort = SortMode::Status;
        let summary = app.summary();
        // Row 2 differs from the majority, row 3 errored; both sort ahead of
        // the agreeing rows.
        assert_eq!(app.display_order(&summary), vec![2, 3, 0, 1]);

        app.sort = SortMode::Time;
        let order = app.display_order(&summary);
        // Equal 10ms answers keep their order; the 3s error row is last.
        assert_eq!(order, vec![0, 1, 2, 3]);
    }

    #[test]
    fn nxdomain_counts_as_responding_and_blocks_full_propagation() {
        // "No such record" is a real propagation signal: that resolver
        // responded, and its view disagrees, so agree < responding.
        let mut app = app_with_answers(&[&["x"], &["x"]]);
        app.rows.push(RowState::Done {
            result: QueryResult::NoRecords("NXDOMAIN".into()),
            elapsed: Duration::from_millis(20),
        });
        let s = app.summary();
        assert_eq!(s.responding, 3);
        assert_eq!(s.agree, 2);
        assert!(s.agree < s.responding);
    }
}
