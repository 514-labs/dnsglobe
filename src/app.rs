use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use hickory_resolver::proto::rr::RecordType;

use crate::dns::{ClientSubnet, QueryOutcome, QueryResult};
use crate::globe::GlobeView;
use crate::resolvers;
use crate::sites::Site;

/// Watch-mode re-poll interval; propagation usually moves on TTL boundaries,
/// so sub-minute polling is plenty.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Slack added on top of a reported TTL before calling a cache stale: we only
/// sample once per POLL_INTERVAL, so an answer can be up to one interval old,
/// plus a little headroom for clock skew and in-flight time.
const TTL_GRACE: Duration = Duration::from_secs(POLL_INTERVAL.as_secs() + 5);

/// Watch-mode observations kept per resolver, oldest dropped first.
const HISTORY_CAP: usize = 32;

/// TTL at or above which the footer suggests lowering it before a planned
/// record change (the "drop TTL to 30s a day before migrating" practice).
pub const ADVISORY_TTL: u32 = 3600;

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

/// Body width at which auto view switches from globe to flat map. The globe
/// panel is square-ish so it stays useful on narrow terminals; the flat map
/// only earns its 350°-wide canvas once there's real room next to the table.
pub const AUTO_FLAT_WIDTH: u16 = 190;

/// Which map panel to show, from `--view`, the config file, or Ctrl+O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    /// Globe on narrow terminals, flat map when the window is wide.
    #[default]
    Auto,
    /// Always the flat map.
    Map,
    /// Always the globe.
    Globe,
}

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
        /// When the answer arrived; anchors the cache-expiry countdown.
        at: Instant,
        /// Whether the resolver honored the round's ECS option (see
        /// `QueryOutcome::ecs_honored`). Always None on ECS-less rounds.
        ecs_honored: Option<bool>,
    },
}

impl RowState {
    /// Time left before this row's reported TTL says the cache entry must be
    /// refetched. None for rows without records.
    pub fn remaining_ttl(&self, now: Instant) -> Option<Duration> {
        let RowState::Done {
            result: QueryResult::Records { min_ttl, .. },
            at,
            ..
        } = self
        else {
            return None;
        };
        let ttl = Duration::from_secs(u64::from(*min_ttl));
        Some(ttl.saturating_sub(now.saturating_duration_since(*at)))
    }
}

/// One watch-mode answer from a resolver, kept to judge cache behavior over
/// successive polls.
#[derive(Debug, Clone)]
struct Observation {
    values: Vec<String>,
    min_ttl: u32,
    at: Instant,
}

/// Why a resolver is still serving a non-majority answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlVerdict {
    /// The answer outlived the TTL the resolver itself reported: the cache is
    /// serving stale data (a resolver that ignores TTLs).
    PastTtl,
    /// The TTL jumped back up while the answer stayed the same: the resolver
    /// did refetch, and *upstream* (e.g. a lagging secondary authoritative
    /// server) still handed out the old data.
    Upstream,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub done: usize,
    pub ok: usize,
    pub no_records: usize,
    pub servfail: usize,
    pub errors: usize,
    /// Resolvers that gave a usable answer (records, an authoritative
    /// "no records", or a SERVFAIL — the resolver's view of the domain is
    /// "broken", a real state during an NS migration). Timeouts and refusals
    /// say nothing about propagation, so percentages are computed against
    /// this, not the full list.
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
    /// Answers from resolvers that ignored the round's ECS option. Shown for
    /// reference but excluded from `responding` — they describe the
    /// resolver's own vantage point, not the probed client subnet, so they
    /// must not drag the propagation percentage (or hold watch mode open)
    /// on GeoDNS zones where their answer legitimately differs.
    pub ecs_blind: usize,
}

/// Parameters of one query round, handed to the spawner in `main`.
pub struct Round {
    pub domain: String,
    pub rtype: RecordType,
    pub ecs: Option<ClientSubnet>,
    pub generation: u64,
    /// Resolver indices to (re)query.
    pub indices: Vec<usize>,
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
    pub queried: Option<(String, RecordType, Option<ClientSubnet>)>,
    /// Client subnets from --ecs/config, cycled with Ctrl+N. Empty for most
    /// runs — every trace of ECS in the UI is gated on this being non-empty.
    pub ecs_list: Vec<ClientSubnet>,
    /// Index into `ecs_list`; None = ECS off (the plain view of the zone).
    pub ecs_sel: Option<usize>,
    /// Table scroll offset; clamped against the viewport during draw.
    pub scroll: usize,
    /// Watch mode: re-poll after each round until propagation reaches 100%.
    /// Enabled by starting a query, toggled with Ctrl+R.
    pub auto_refresh: bool,
    /// When the next poll fires, if one is scheduled.
    pub next_poll: Option<Instant>,
    /// Active table ordering, cycled with Ctrl+S.
    pub sort: SortMode,
    /// Flat map ↔ rotating globe morph state.
    pub globe: GlobeView,
    /// View policy: auto by width, or pinned by --view/config/Ctrl+O.
    pub view_mode: ViewMode,
    /// False until the first `sync_view`: the first frame snaps to its view
    /// instead of replaying the morph on every launch in a narrow terminal.
    view_synced: bool,
    /// Per-resolver anycast site discovered by that operator's identification
    /// query (issue #6): which POP is actually answering us. None = no probe
    /// or probe failed. Session-static — the site depends on our network
    /// path, not on the queried domain.
    pub sites: Vec<Option<Site>>,
    /// Per-resolver answers across watch-mode polls (bounded FIFO). Cleared
    /// on a fresh query, preserved across re-polls — cache-behavior verdicts
    /// only exist while watching one domain/type.
    history: Vec<VecDeque<Observation>>,
}

impl App {
    pub fn new(domain: String) -> Self {
        Self {
            cursor: domain.len(),
            domain,
            rtype_idx: 0,
            rows: vec![RowState::Idle; resolvers::active().len()],
            generation: 0,
            spinner_frame: 0,
            should_quit: false,
            queried: None,
            ecs_list: Vec::new(),
            ecs_sel: None,
            scroll: 0,
            auto_refresh: false,
            next_poll: None,
            sort: SortMode::default(),
            globe: GlobeView::new(Instant::now()),
            view_mode: ViewMode::default(),
            view_synced: false,
            sites: vec![None; resolvers::active().len()],
            history: vec![VecDeque::new(); resolvers::active().len()],
        }
    }

    /// Where this resolver's answers come from, as shown in the Loc column:
    /// the discovered anycast site when known, else the configured location.
    pub fn effective_location(&self, index: usize) -> &str {
        match &self.sites[index] {
            Some(site) => &site.code,
            None => &resolvers::active()[index].location,
        }
    }

    /// Map position for this resolver: the discovered site when we know its
    /// coordinates, else the configured (operator home) position.
    pub fn effective_coords(&self, index: usize) -> Option<(f64, f64)> {
        self.sites[index]
            .as_ref()
            .and_then(|site| site.coords)
            .or(resolvers::active()[index].coords)
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

    /// Jump to the start of the current dot-separated label, or of the
    /// previous one when already at a label boundary.
    pub fn move_cursor_word_left(&mut self) {
        let bytes = self.domain.as_bytes();
        while self.cursor > 0 && bytes[self.cursor - 1] == b'.' {
            self.cursor -= 1;
        }
        while self.cursor > 0 && bytes[self.cursor - 1] != b'.' {
            self.cursor -= 1;
        }
    }

    /// Jump past the end of the current dot-separated label, or of the next
    /// one when already at a label boundary.
    pub fn move_cursor_word_right(&mut self) {
        let bytes = self.domain.as_bytes();
        while self.cursor < bytes.len() && bytes[self.cursor] == b'.' {
            self.cursor += 1;
        }
        while self.cursor < bytes.len() && bytes[self.cursor] != b'.' {
            self.cursor += 1;
        }
    }

    pub fn clear_domain(&mut self) {
        self.domain.clear();
        self.cursor = 0;
    }

    /// The view this width calls for under the active policy.
    pub fn desired_globe(&self, body_width: u16) -> bool {
        match self.view_mode {
            ViewMode::Globe => true,
            ViewMode::Map => false,
            ViewMode::Auto => body_width < AUTO_FLAT_WIDTH,
        }
    }

    /// Re-assert the view target for the current width; called every frame
    /// so resizing across the auto threshold morphs the panel. The first
    /// call snaps (no launch animation), later changes animate.
    pub fn sync_view(&mut self, body_width: u16) {
        let want = self.desired_globe(body_width);
        if self.view_synced {
            self.globe.set_target(want, Instant::now());
        } else {
            self.globe.snap(want);
            self.view_synced = true;
        }
    }

    /// Ctrl+O: flip the view and pin it — a manual choice shouldn't be
    /// overridden by the next resize.
    pub fn toggle_globe(&mut self) {
        self.view_mode = if self.globe.target() {
            ViewMode::Map
        } else {
            ViewMode::Globe
        };
        self.globe
            .set_target(self.view_mode == ViewMode::Globe, Instant::now());
    }

    pub fn cycle_record_type(&mut self, forward: bool) {
        let n = RECORD_TYPES.len();
        self.rtype_idx = if forward {
            (self.rtype_idx + 1) % n
        } else {
            (self.rtype_idx + n - 1) % n
        };
    }

    /// Install the ECS list from config/CLI. Selection starts on the first
    /// subnet — passing --ecs means "query with it", not just "have it
    /// available".
    pub fn set_ecs_list(&mut self, list: Vec<ClientSubnet>) {
        self.ecs_sel = (!list.is_empty()).then_some(0);
        self.ecs_list = list;
    }

    /// The subnet the next Enter will query with.
    pub fn active_ecs(&self) -> Option<ClientSubnet> {
        self.ecs_sel.map(|i| self.ecs_list[i])
    }

    /// Ctrl+N: step the selection through the configured subnets plus an
    /// "off" position. The caller follows up with `begin_reselect` so the
    /// table refreshes for the new subnet without waiting for Enter.
    pub fn cycle_ecs(&mut self) {
        if self.ecs_list.is_empty() {
            return;
        }
        self.ecs_sel = match self.ecs_sel {
            Some(i) if i + 1 < self.ecs_list.len() => Some(i + 1),
            Some(_) => None, // past the last subnet: ECS off
            None => Some(0),
        };
    }

    /// Arm a new query round. Returns what to query (all resolvers), or None
    /// if the domain input is empty.
    pub fn begin_query(&mut self) -> Option<Round> {
        let domain = self.domain.trim().trim_end_matches('.').to_string();
        if domain.is_empty() {
            return None;
        }
        Some(self.arm_round(domain, self.record_type()))
    }

    /// Arm a fresh round for the already-queried domain with the current
    /// record-type and ECS selections: Tab and Ctrl+N re-query as they
    /// cycle. Reads `queried`'s domain, not the (possibly mid-edit) input
    /// field, and does nothing before the first query — there's nothing to
    /// refresh yet.
    pub fn begin_reselect(&mut self) -> Option<Round> {
        let (domain, ..) = self.queried.clone()?;
        Some(self.arm_round(domain, self.record_type()))
    }

    /// Reset every row and start a round of all resolvers, capturing the
    /// active ECS selection. History clears too: answers under a different
    /// subnet aren't comparable across polls.
    fn arm_round(&mut self, domain: String, rtype: RecordType) -> Round {
        self.generation += 1;
        self.rows = vec![RowState::Pending; resolvers::active().len()];
        self.history = vec![VecDeque::new(); resolvers::active().len()];
        let ecs = self.active_ecs();
        self.queried = Some((domain.clone(), rtype, ecs));
        Round {
            domain,
            rtype,
            ecs,
            generation: self.generation,
            indices: (0..self.rows.len()).collect(),
        }
    }

    /// Arm a poll of the last-queried domain/type, ignoring the (possibly
    /// mid-edit) input field. Only re-polls rows whose answer can have
    /// changed: rows agreeing with the majority are skipped while their TTL
    /// countdown still runs (a cache can't legally change before expiry), and
    /// picked up again once it hits zero — so an old-value *majority* still
    /// gets re-checked and can flip.
    pub fn begin_requery(&mut self) -> Option<Round> {
        // Re-polls stay on the queried round's ECS subnet (Ctrl+N re-arms
        // `queried` via begin_reselect, so the two can't drift) — mixing
        // subnets within one table would make the group comparison
        // meaningless.
        let (domain, rtype, ecs) = self.queried.clone()?;
        let summary = self.summary();
        let now = Instant::now();
        let indices: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let agreeing = matches!(
                    &self.rows[i],
                    RowState::Done {
                        result: QueryResult::Records { .. },
                        ..
                    }
                ) && summary.majority_rows[i];
                !(agreeing
                    && self.rows[i]
                        .remaining_ttl(now)
                        .is_some_and(|r| !r.is_zero()))
            })
            .collect();
        if indices.is_empty() {
            return None;
        }
        self.generation += 1;
        for &i in &indices {
            self.rows[i] = RowState::Pending;
        }
        Some(Round {
            domain,
            rtype,
            ecs,
            generation: self.generation,
            indices,
        })
    }

    pub fn apply(&mut self, outcome: QueryOutcome) {
        if outcome.generation != self.generation {
            return; // stale result from a superseded query round
        }
        let now = Instant::now();
        if let QueryResult::Records { values, min_ttl } = &outcome.result {
            let history = &mut self.history[outcome.resolver_index];
            if history.len() == HISTORY_CAP {
                history.pop_front();
            }
            history.push_back(Observation {
                values: values.clone(),
                min_ttl: *min_ttl,
                at: now,
            });
        }
        self.rows[outcome.resolver_index] = RowState::Done {
            result: outcome.result,
            elapsed: outcome.elapsed,
            at: now,
            ecs_honored: outcome.ecs_honored,
        };
    }

    /// Judge a resolver's cache behavior from its answer history. Only
    /// meaningful for rows currently *disagreeing* with the majority — the
    /// caller filters; the same patterns on an agreeing row are normal
    /// operation.
    pub fn ttl_verdict(&self, index: usize, now: Instant) -> Option<TtlVerdict> {
        let history = &self.history[index];
        let latest = history.back()?;
        // Tail streak of identical answers; one sample proves nothing.
        let streak = history
            .iter()
            .rev()
            .take_while(|o| o.values == latest.values)
            .count();
        if streak < 2 {
            return None;
        }
        let first = &history[history.len() - streak];
        // TTL rising within the streak means the resolver refetched and got
        // the same old data back: the lag is upstream, not this cache.
        let mut prev_ttl = first.min_ttl;
        for obs in history.iter().skip(history.len() - streak + 1) {
            if obs.min_ttl > prev_ttl {
                return Some(TtlVerdict::Upstream);
            }
            prev_ttl = obs.min_ttl;
        }
        let deadline = first.at + Duration::from_secs(u64::from(first.min_ttl)) + TTL_GRACE;
        (now > deadline).then_some(TtlVerdict::PastTtl)
    }

    /// Estimated configured TTL: the max reported TTL across majority rows.
    /// A resolver that just refetched reports (nearly) the full configured
    /// value, so the max over the fleet is within seconds of the zone's TTL.
    pub fn estimated_ttl(&self, summary: &Summary) -> Option<u32> {
        self.rows
            .iter()
            .enumerate()
            .filter(|&(i, _)| summary.majority_rows[i])
            .filter_map(|(_, row)| match row {
                RowState::Done {
                    result: QueryResult::Records { min_ttl, .. },
                    ..
                } => Some(*min_ttl),
                _ => None,
            })
            .max()
    }

    /// Worst-case wait until every non-majority cache must have refetched:
    /// the max remaining TTL across differing rows. None when nothing
    /// differs (or differing rows carry no records).
    pub fn stale_expiry_bound(&self, summary: &Summary, now: Instant) -> Option<Duration> {
        self.rows
            .iter()
            .enumerate()
            .filter(|&(i, row)| {
                !summary.majority_rows[i]
                    && matches!(
                        row,
                        RowState::Done {
                            result: QueryResult::Records { .. },
                            ..
                        }
                    )
            })
            .filter_map(|(_, row)| row.remaining_ttl(now))
            .max()
    }

    pub fn in_flight(&self) -> bool {
        self.rows.iter().any(|r| matches!(r, RowState::Pending))
    }

    /// Resolver indices in display order under the active sort.
    pub fn display_order(&self, summary: &Summary) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.rows.len()).collect();
        match self.sort {
            SortMode::Resolver => {}
            SortMode::Location => order.sort_by_key(|&i| self.effective_location(i)),
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
                let now = Instant::now();
                order.sort_by_key(|&i| match &self.rows[i] {
                    RowState::Done { result, .. } => match result {
                        QueryResult::Records { .. } if in_flight || summary.majority_rows[i] => 5,
                        // Misbehaving caches are the most actionable rows.
                        QueryResult::Records { .. } if self.ttl_verdict(i, now).is_some() => 0,
                        QueryResult::Records { .. } => 1, // differs
                        QueryResult::NoRecords(_) => 2,
                        QueryResult::ServFail => 3,
                        QueryResult::Error(_) => 4,
                    },
                    RowState::Pending => 6,
                    RowState::Idle => 7,
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
            let RowState::Done {
                result,
                ecs_honored,
                ..
            } = row
            else {
                continue;
            };
            summary.done += 1;
            // An answer that ignored the round's ECS option is a different
            // question's answer: keep it out of the propagation math (see
            // `Summary::ecs_blind`). Errors and SERVFAIL keep their normal
            // handling — they carry no echo either way.
            if *ecs_honored == Some(false)
                && matches!(
                    result,
                    QueryResult::Records { .. } | QueryResult::NoRecords(_)
                )
            {
                summary.ecs_blind += 1;
                continue;
            }
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
                QueryResult::ServFail => summary.servfail += 1,
                QueryResult::Error(_) => summary.errors += 1,
            }
        }
        summary.responding = summary.ok + summary.no_records + summary.servfail;

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

/// Compact human duration for countdowns and TTLs: `42s`, `4m10s`, `23h59m`,
/// `2d3h`. Two units max keeps it within a narrow table column.
pub fn fmt_secs(total: u64) -> String {
    let (days, hours, mins, secs) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
        total % 60,
    );
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{secs:02}s")
    } else {
        format!("{secs}s")
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
                at: Instant::now(),
                ecs_honored: None,
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
        let answers = vec![&["x"] as &[&str]; resolvers::active().len()];
        let app = app_with_answers(&answers);
        let s = app.summary();
        assert_eq!(s.responding, resolvers::active().len());
        assert_eq!(s.agree, s.responding);
    }

    #[test]
    fn unreachable_resolvers_do_not_block_full_propagation() {
        // Refused/timed-out resolvers carry no signal: with one error row,
        // the rest agreeing still counts as 100% (agree == responding).
        let mut app = app_with_answers(&vec![&["x"] as &[&str]; resolvers::active().len() - 1]);
        app.rows.push(RowState::Done {
            result: QueryResult::Error("refused".into()),
            elapsed: Duration::from_secs(3),
            at: Instant::now(),
            ecs_honored: None,
        });
        let s = app.summary();
        assert_eq!(s.groups, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(s.responding, resolvers::active().len() - 1);
        assert_eq!(s.agree, s.responding);
    }

    #[test]
    fn location_sort_groups_locations_and_keeps_curated_order_within() {
        let mut app = app_with_answers(&vec![&["x"] as &[&str]; resolvers::active().len()]);
        app.sort = SortMode::Location;
        let summary = app.summary();
        let order = app.display_order(&summary);
        let locations: Vec<&str> = order
            .iter()
            .map(|&i| resolvers::active()[i].location.as_str())
            .collect();
        assert!(locations.is_sorted());
        // Stable sort: within one location, curated order is preserved.
        let us: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| resolvers::active()[i].location == "US")
            .collect();
        assert!(us.is_sorted());
    }

    #[test]
    fn status_sort_puts_problems_first_and_time_sort_fastest_first() {
        let mut app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        app.rows.push(RowState::Done {
            result: QueryResult::Error("timeout".into()),
            elapsed: Duration::from_secs(3),
            at: Instant::now(),
            ecs_honored: None,
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

    /// A `now` safely in the future so tests can place observations "in the
    /// past" relative to it without risking Instant underflow on a
    /// freshly-booted machine.
    fn future_now() -> Instant {
        Instant::now() + Duration::from_secs(100_000)
    }

    fn obs(values: &[&str], min_ttl: u32, now: Instant, ago: u64) -> Observation {
        Observation {
            values: values.iter().map(|v| v.to_string()).collect(),
            min_ttl,
            at: now - Duration::from_secs(ago),
        }
    }

    #[test]
    fn word_left_jumps_to_label_starts() {
        let mut app = App::new("api.example.com".into());
        app.cursor = app.domain.len();
        let stops: Vec<usize> = std::iter::from_fn(|| {
            app.move_cursor_word_left();
            Some(app.cursor)
        })
        .take(4)
        .collect();
        // "api.example.com": label starts at 12 ("com"), 4 ("example"),
        // 0 ("api"), then stays put.
        assert_eq!(stops, vec![12, 4, 0, 0]);
    }

    #[test]
    fn word_right_jumps_to_label_ends() {
        let mut app = App::new("api.example.com".into());
        app.cursor = 0;
        let stops: Vec<usize> = std::iter::from_fn(|| {
            app.move_cursor_word_right();
            Some(app.cursor)
        })
        .take(4)
        .collect();
        assert_eq!(stops, vec![3, 11, 15, 15]);
    }

    #[test]
    fn word_motion_skips_consecutive_dots() {
        let mut app = App::new("a..b".into());
        app.cursor = 4;
        app.move_cursor_word_left();
        assert_eq!(app.cursor, 3); // start of "b"
        app.move_cursor_word_left();
        assert_eq!(app.cursor, 0); // past both dots to start of "a"
        app.move_cursor_word_right();
        assert_eq!(app.cursor, 1); // end of "a"
        app.move_cursor_word_right();
        assert_eq!(app.cursor, 4); // past both dots to end of "b"
    }

    #[test]
    fn auto_view_picks_globe_below_the_width_threshold() {
        let app = App::new("example.com".into());
        assert!(app.desired_globe(AUTO_FLAT_WIDTH - 1));
        assert!(!app.desired_globe(AUTO_FLAT_WIDTH));
    }

    #[test]
    fn forced_views_ignore_the_width() {
        let mut app = App::new("example.com".into());
        app.view_mode = ViewMode::Globe;
        assert!(app.desired_globe(500));
        app.view_mode = ViewMode::Map;
        assert!(!app.desired_globe(100));
    }

    #[test]
    fn first_sync_snaps_then_resizes_animate() {
        let mut app = App::new("example.com".into());
        // Launching in a narrow terminal starts on the globe instantly.
        app.sync_view(120);
        assert!((app.globe.t(Instant::now()) - 1.0).abs() < 1e-9);
        // Widening past the threshold animates back toward the flat map:
        // target flips but the morph has barely moved yet.
        app.sync_view(300);
        assert!(!app.globe.target());
        assert!(app.globe.t(Instant::now()) > 0.9);
    }

    #[test]
    fn manual_toggle_pins_the_view_against_resizes() {
        let mut app = App::new("example.com".into());
        app.sync_view(300); // auto, wide → flat map
        assert!(!app.globe.target());
        app.toggle_globe();
        assert_eq!(app.view_mode, ViewMode::Globe);
        // Still globe after re-syncing at a width auto would call flat.
        app.sync_view(300);
        assert!(app.globe.target());
        app.toggle_globe();
        assert_eq!(app.view_mode, ViewMode::Map);
        app.sync_view(120);
        assert!(!app.globe.target());
    }

    #[test]
    fn fmt_secs_is_compact_two_units() {
        assert_eq!(fmt_secs(42), "42s");
        assert_eq!(fmt_secs(250), "4m10s");
        assert_eq!(fmt_secs(3600), "1h00m");
        assert_eq!(fmt_secs(86_399), "23h59m");
        assert_eq!(fmt_secs(90_000), "1d1h");
    }

    #[test]
    fn verdict_needs_at_least_two_observations() {
        let mut app = App::new("example.com".into());
        let now = future_now();
        app.history[0] = VecDeque::from([obs(&["old"], 60, now, 500)]);
        assert_eq!(app.ttl_verdict(0, now), None);
    }

    #[test]
    fn same_answer_past_reported_ttl_is_a_stale_cache() {
        let mut app = App::new("example.com".into());
        let now = future_now();
        // First seen 120s ago with a 60s TTL: deadline (60s + grace) passed,
        // TTL never rose, answer unchanged → the cache is ignoring its TTL.
        app.history[0] = VecDeque::from([obs(&["old"], 60, now, 120), obs(&["old"], 60, now, 10)]);
        assert_eq!(app.ttl_verdict(0, now), Some(TtlVerdict::PastTtl));
    }

    #[test]
    fn same_answer_within_ttl_is_no_verdict() {
        let mut app = App::new("example.com".into());
        let now = future_now();
        app.history[0] = VecDeque::from([obs(&["old"], 300, now, 60), obs(&["old"], 300, now, 10)]);
        assert_eq!(app.ttl_verdict(0, now), None);
    }

    #[test]
    fn ttl_rising_with_same_answer_means_upstream_lag() {
        let mut app = App::new("example.com".into());
        let now = future_now();
        // TTL jumped 60 → 300: the resolver refetched and the authority
        // handed the old data back — not this cache's fault.
        app.history[0] = VecDeque::from([obs(&["old"], 60, now, 100), obs(&["old"], 300, now, 40)]);
        assert_eq!(app.ttl_verdict(0, now), Some(TtlVerdict::Upstream));
    }

    #[test]
    fn answer_change_resets_the_streak() {
        let mut app = App::new("example.com".into());
        let now = future_now();
        // Old answer lingered way past TTL, but the *current* answer is one
        // fresh sample: no verdict against the new streak.
        app.history[0] = VecDeque::from([
            obs(&["old"], 60, now, 500),
            obs(&["old"], 60, now, 400),
            obs(&["new"], 60, now, 10),
        ]);
        assert_eq!(app.ttl_verdict(0, now), None);
    }

    #[test]
    fn history_is_a_bounded_fifo() {
        let mut app = App::new("example.com".into());
        for i in 0..(HISTORY_CAP + 5) {
            app.apply(QueryOutcome {
                resolver_index: 0,
                generation: 0,
                result: QueryResult::Records {
                    values: vec![format!("v{i}")],
                    min_ttl: 60,
                },
                elapsed: Duration::from_millis(10),
                ecs_honored: None,
            });
        }
        assert_eq!(app.history[0].len(), HISTORY_CAP);
        // Oldest entries were dropped first.
        assert_eq!(app.history[0].front().unwrap().values, vec!["v5"]);
    }

    #[test]
    fn requery_skips_agreeing_rows_until_their_ttl_expires() {
        let mut app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        app.queried = Some(("example.com".into(), RecordType::A, None));
        // Majority rows are fresh (60s TTL): only the differing row re-polls.
        let round = app.begin_requery().unwrap();
        assert_eq!(round.indices, vec![2]);
        assert!(matches!(app.rows[2], RowState::Pending));
        assert!(matches!(app.rows[0], RowState::Done { .. }));
    }

    #[test]
    fn requery_repolls_agreeing_rows_once_expired_and_all_errors() {
        let mut app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        // Row 0's cache entry has expired (TTL 0): even though it agrees,
        // its answer can now legally change, so it re-polls.
        if let RowState::Done {
            result: QueryResult::Records { min_ttl, .. },
            ..
        } = &mut app.rows[0]
        {
            *min_ttl = 0;
        }
        app.rows.push(RowState::Done {
            result: QueryResult::Error("timeout".into()),
            elapsed: Duration::from_secs(3),
            at: Instant::now(),
            ecs_honored: None,
        });
        app.queried = Some(("example.com".into(), RecordType::A, None));
        let round = app.begin_requery().unwrap();
        assert_eq!(round.indices, vec![0, 2, 3]);
    }

    #[test]
    fn estimated_ttl_is_max_over_majority_rows_only() {
        let mut app = app_with_answers(&[&["x"], &["x"], &["y"]]);
        let ttls = [300u32, 3600, 999_999];
        for (row, ttl) in app.rows.iter_mut().zip(ttls) {
            if let RowState::Done {
                result: QueryResult::Records { min_ttl, .. },
                ..
            } = row
            {
                *min_ttl = ttl;
            }
        }
        let summary = app.summary();
        // The differing row's huge TTL must not leak into the estimate.
        assert_eq!(app.estimated_ttl(&summary), Some(3600));
    }

    #[test]
    fn stale_expiry_bound_covers_only_differing_rows() {
        let app = app_with_answers(&[&["new"], &["new"], &["old"]]);
        let summary = app.summary();
        let bound = app.stale_expiry_bound(&summary, Instant::now()).unwrap();
        // The differing row was just answered with a 60s TTL.
        assert!(bound <= Duration::from_secs(60));
        assert!(bound > Duration::from_secs(50));

        // Full agreement → nothing stale → no bound.
        let app = app_with_answers(&[&["x"], &["x"]]);
        let summary = app.summary();
        assert_eq!(app.stale_expiry_bound(&summary, Instant::now()), None);
    }

    #[test]
    fn servfail_counts_as_responding_and_blocks_full_propagation() {
        // A resolver stuck on a delegation whose nameservers were deleted
        // answers SERVFAIL: that's "not propagated here", not noise — it must
        // hold the percentage below 100% and keep watch mode polling.
        let mut app = app_with_answers(&[&["x"], &["x"]]);
        app.rows.push(RowState::Done {
            result: QueryResult::ServFail,
            elapsed: Duration::from_millis(20),
            at: Instant::now(),
            ecs_honored: None,
        });
        let s = app.summary();
        assert_eq!(s.servfail, 1);
        assert_eq!(s.errors, 0);
        assert_eq!(s.responding, 3);
        assert_eq!(s.agree, 2);
        assert!(s.agree < s.responding);
    }

    #[test]
    fn status_sort_ranks_servfail_between_no_records_and_errors() {
        let mut app = app_with_answers(&[&["x"]]);
        for result in [
            QueryResult::Error("timeout".into()),
            QueryResult::ServFail,
            QueryResult::NoRecords("NXDOMAIN".into()),
        ] {
            app.rows.push(RowState::Done {
                result,
                elapsed: Duration::from_millis(20),
                at: Instant::now(),
                ecs_honored: None,
            });
        }
        app.sort = SortMode::Status;
        let summary = app.summary();
        assert_eq!(app.display_order(&summary), vec![3, 2, 1, 0]);
    }

    #[test]
    fn ecs_cycling_steps_through_subnets_and_off() {
        let mut app = App::new("example.com".into());
        // No list configured: Ctrl+N is inert and ECS stays off.
        app.cycle_ecs();
        assert_eq!(app.active_ecs(), None);

        let list = vec![
            crate::dns::parse_ecs("203.0.113.0/24").unwrap(),
            crate::dns::parse_ecs("198.51.100.0/24").unwrap(),
        ];
        app.set_ecs_list(list.clone());
        // Configuring ECS means querying with it: selection starts on the
        // first subnet, then cycles through the rest, off, and around.
        assert_eq!(app.active_ecs(), Some(list[0]));
        app.cycle_ecs();
        assert_eq!(app.active_ecs(), Some(list[1]));
        app.cycle_ecs();
        assert_eq!(app.active_ecs(), None);
        app.cycle_ecs();
        assert_eq!(app.active_ecs(), Some(list[0]));
    }

    #[test]
    fn requery_keeps_the_queried_rounds_subnet_despite_cycling() {
        let subnet = crate::dns::parse_ecs("203.0.113.0/24").unwrap();
        let mut app = App::new("example.com".into());
        app.set_ecs_list(vec![subnet]);
        let round = app.begin_query().unwrap();
        assert_eq!(round.ecs, Some(subnet));

        // If a watch poll fires between cycling and the Ctrl+N requery, it
        // must stay on the queried round's subnet: mixing subnets within
        // one table would break the group comparison.
        app.cycle_ecs();
        assert_eq!(app.active_ecs(), None);
        let round = app.begin_requery().unwrap();
        assert_eq!(round.ecs, Some(subnet));
        // A fresh Enter picks up the new selection.
        let round = app.begin_query().unwrap();
        assert_eq!(round.ecs, None);
    }

    #[test]
    fn ctrl_n_requeries_the_queried_domain_with_the_new_subnet() {
        let list = vec![
            crate::dns::parse_ecs("203.0.113.0/24").unwrap(),
            crate::dns::parse_ecs("198.51.100.0/24").unwrap(),
        ];
        let mut app = App::new("example.com".into());
        app.set_ecs_list(list.clone());

        // Before any query there's nothing to refresh: cycling alone.
        app.cycle_ecs();
        assert!(app.begin_reselect().is_none());
        app.cycle_ecs(); // back around: off
        app.cycle_ecs(); // first subnet again
        assert_eq!(app.active_ecs(), Some(list[0]));

        let first = app.begin_query().unwrap();
        // Ctrl+N: cycle, then a fresh full round on the *queried* domain —
        // even while the input field is mid-edit.
        app.insert_char('x');
        app.cycle_ecs();
        let round = app.begin_reselect().unwrap();
        assert_eq!(round.domain, "example.com");
        assert_eq!(round.ecs, Some(list[1]));
        assert!(round.generation > first.generation);
        assert_eq!(round.indices.len(), resolvers::active().len());
        // The new round is what re-polls now follow.
        assert_eq!(app.queried.as_ref().unwrap().2, Some(list[1]));
    }

    #[test]
    fn tab_requeries_with_the_new_record_type() {
        let mut app = App::new("example.com".into());
        // Nothing queried yet: cycling the type refreshes nothing.
        app.cycle_record_type(true);
        assert!(app.begin_reselect().is_none());

        app.begin_query().unwrap();
        app.insert_char('x'); // mid-edit input must not leak into the round
        app.cycle_record_type(true);
        let round = app.begin_reselect().unwrap();
        assert_eq!(round.domain, "example.com");
        assert_eq!(round.rtype, RECORD_TYPES[2]); // A → AAAA → CNAME
        // Re-polls follow the new type.
        assert_eq!(app.queried.as_ref().unwrap().1, RECORD_TYPES[2]);
    }

    #[test]
    fn ecs_blind_answers_are_excluded_from_propagation() {
        // Two resolvers honored the subnet and agree; one (e.g. Cloudflare)
        // ignored ECS and shows its own vantage point's answer. That row
        // must not block 100% propagation — on GeoDNS it may never match.
        let mut app = app_with_answers(&[&["geo"], &["geo"]]);
        for row in &mut app.rows {
            if let RowState::Done { ecs_honored, .. } = row {
                *ecs_honored = Some(true);
            }
        }
        app.rows.push(RowState::Done {
            result: QueryResult::Records {
                values: vec!["other".into()],
                min_ttl: 60,
            },
            elapsed: Duration::from_millis(10),
            at: Instant::now(),
            ecs_honored: Some(false),
        });
        app.rows.push(RowState::Done {
            result: QueryResult::NoRecords("NXDomain".into()),
            elapsed: Duration::from_millis(10),
            at: Instant::now(),
            ecs_honored: Some(false),
        });
        let s = app.summary();
        assert_eq!(s.ecs_blind, 2);
        assert_eq!(s.ok, 2);
        assert_eq!(s.no_records, 0);
        assert_eq!(s.groups, 1);
        assert_eq!(s.responding, 2);
        assert_eq!(s.agree, s.responding); // watch mode can complete
        assert_eq!(s.majority_rows, vec![true, true, false, false]);
    }

    #[test]
    fn nxdomain_counts_as_responding_and_blocks_full_propagation() {
        // "No such record" is a real propagation signal: that resolver
        // responded, and its view disagrees, so agree < responding.
        let mut app = app_with_answers(&[&["x"], &["x"]]);
        app.rows.push(RowState::Done {
            result: QueryResult::NoRecords("NXDOMAIN".into()),
            elapsed: Duration::from_millis(20),
            at: Instant::now(),
            ecs_honored: None,
        });
        let s = app.summary();
        assert_eq!(s.responding, 3);
        assert_eq!(s.agree, 2);
        assert!(s.agree < s.responding);
    }
}
