// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Lease scope globs, overlap detection, and toe-step warnings.
//!
//! **A lease is knowledge, not a lock.** Everything in this file is
//! advisory: overlap detection exists to warn at declaration time — the
//! moment coordination is still cheap — never to block. Because a false
//! positive costs one ignorable warning and a false negative costs nothing
//! that git wouldn't also have cost, the overlap check is deliberately
//! **approximate** (documented limits below).
//!
//! Glob grammar (hand-rolled; no new dependency):
//! * `**` — any number of path segments, including zero
//! * `*`  — any run of characters within one segment
//! * `?`  — one character within a segment
//! * a fully-literal pattern matches that exact file OR any path under it
//!   (so `src` leases the whole directory, which is what people mean)
//!
//! Character classes (`[ab]`), brace sets and negation are NOT supported —
//! a `[` is just a bracket. Overlap approximation: two patterns are said to
//! overlap when walking their segments never hits two *literal* segments
//! that differ; any wildcard-containing segment is assumed to co-match, and
//! `**` on either side ends the walk as an overlap. This over-reports
//! (e.g. `src/a*` vs `src/b*` is called an overlap) and never under-reports
//! for the supported grammar.

use super::{cap, Lease, RepoState, ToeStep, MAX_PATTERN_CHARS, MAX_SCOPE_PATTERNS};

/// Validate and normalize scope patterns: relative, slash-separated, no
/// `..`, no leading `/`, bounded in count and length.
pub fn validate_scope(scope: &[String]) -> Result<Vec<String>, String> {
    if scope.is_empty() {
        return Err("a lease needs at least one scope pattern (path glob)".into());
    }
    if scope.len() > MAX_SCOPE_PATTERNS {
        return Err(format!("at most {MAX_SCOPE_PATTERNS} scope patterns per lease"));
    }
    let mut out = Vec::new();
    for raw in scope {
        let p = raw.trim().trim_start_matches("./").to_string();
        if p.is_empty() || p.chars().count() > MAX_PATTERN_CHARS {
            return Err(format!("bad scope pattern {raw:?}: empty or too long"));
        }
        if p.starts_with('/') || p.contains('\\') {
            return Err(format!(
                "bad scope pattern {p:?}: use repo-relative, slash-separated globs"
            ));
        }
        if p.split('/').any(|seg| seg == ".." || seg.is_empty()) {
            return Err(format!("bad scope pattern {p:?}: no '..' or empty segments"));
        }
        out.push(p);
    }
    Ok(out)
}

/// Does `pattern` match the repo-relative `path` (a FILE path)?
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    if match_segs(&p, &s) {
        return true;
    }
    // A fully-literal pattern also matches as a directory prefix.
    if p.iter().all(|seg| is_literal(seg)) && s.len() > p.len() && s[..p.len()] == p[..] {
        return true;
    }
    false
}

fn match_segs(p: &[&str], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => match_segs(&p[1..], s) || (!s.is_empty() && match_segs(p, &s[1..])),
        Some(seg) => {
            !s.is_empty() && match_one(seg, s[0]) && match_segs(&p[1..], &s[1..])
        }
    }
}

/// `*`/`?` matching within one segment.
fn match_one(pattern: &str, seg: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = seg.chars().collect();
    go(&p, &s)
}

fn is_literal(seg: &str) -> bool {
    !seg.contains('*') && !seg.contains('?')
}

/// The approximate pattern-vs-pattern overlap test (limits in the module
/// doc). Symmetric; conservative toward "yes".
pub fn patterns_may_overlap(a: &str, b: &str) -> bool {
    let pa: Vec<&str> = a.split('/').collect();
    let pb: Vec<&str> = b.split('/').collect();
    let mut i = 0;
    loop {
        match (pa.get(i), pb.get(i)) {
            (None, None) => return true,
            // One pattern consumed: it names exact-depth paths — unless it
            // is fully literal, in which case it doubles as a directory
            // prefix and everything deeper overlaps it.
            (None, Some(_)) => return pa.iter().all(|s| is_literal(s)),
            (Some(_), None) => return pb.iter().all(|s| is_literal(s)),
            (Some(&"**"), Some(_)) | (Some(_), Some(&"**")) => return true,
            (Some(x), Some(y)) => {
                if is_literal(x) && is_literal(y) && x != y {
                    return false;
                }
                // Wildcard-containing segments are assumed to co-match.
                i += 1;
            }
        }
    }
}

/// The literal directory prefix of a pattern: segments before the first
/// wildcard-containing one. `src/parse/**` → `src/parse`; `**` → ``.
pub fn literal_prefix(pattern: &str) -> String {
    pattern
        .split('/')
        .take_while(|seg| is_literal(seg))
        .collect::<Vec<_>>()
        .join("/")
}

/// Compare a candidate lease against every LIVE lease in the repo and
/// record one toe-step per colliding lease (first colliding pattern pair
/// wins — one warning per pair of goals, not per pattern product).
pub fn detect_toe_steps(rs: &RepoState, candidate: &Lease, now_ms: u64) -> Vec<ToeStep> {
    let mut out = Vec::new();
    for other in &rs.leases {
        if other.id == candidate.id || other.expired(now_ms) {
            continue;
        }
        // Only leases whose thread is still live can be stepped on.
        let live = rs
            .threads
            .iter()
            .any(|t| t.lease_id.as_deref() == Some(&other.id) && t.status.is_live());
        if !live {
            continue;
        }
        let collision = candidate.scope.iter().find_map(|a| {
            other
                .scope
                .iter()
                .find(|b| patterns_may_overlap(a, b))
                .map(|b| (a.clone(), b.clone()))
        });
        if let Some((pat_a, pat_b)) = collision {
            out.push(ToeStep {
                id: format!("toe-{now_ms}-{}", out.len() + rs.toe_steps.len() + 1),
                ts_ms: now_ms,
                lease_a: candidate.id.clone(),
                lease_b: other.id.clone(),
                goal_a: candidate.goal.clone(),
                goal_b: other.goal.clone(),
                pattern_a: pat_a.clone(),
                pattern_b: pat_b.clone(),
                suggested_split: suggest_split(&pat_a, &pat_b),
            });
        }
    }
    out
}

/// Suggest a non-overlapping split for two colliding patterns: name the
/// shared literal prefix and propose each side take a DISTINCT literal
/// subtree under it. Mechanical, not clever — the point is to hand both
/// agents the same concrete starting sentence for a renegotiation.
pub fn suggest_split(a: &str, b: &str) -> Vec<String> {
    if a == b {
        return vec![format!(
            "both leases name exactly {} — coordinate directly or take turns; \
             the later weave will be asked to rebase",
            cap(a, 80)
        )];
    }
    let la = literal_prefix(a);
    let lb = literal_prefix(b);
    if !la.is_empty() && !lb.is_empty() && la != lb && !la.starts_with(&lb) && !lb.starts_with(&la)
    {
        // Distinct literal prefixes already exist — the split writes itself.
        return vec![
            format!("one thread keeps {la}/**"),
            format!("the other keeps {lb}/**"),
        ];
    }
    let common = if la.len() <= lb.len() { la.clone() } else { lb.clone() };
    let root = if common.is_empty() { "the repo root".to_string() } else { format!("{common}/") };
    vec![
        format!(
            "both scopes reach into {root} — split it into disjoint literal subtrees, \
             e.g. {p}<area-one>/** vs {p}<area-two>/**",
            p = if common.is_empty() { String::new() } else { format!("{common}/") },
        ),
        format!(
            "colliding patterns were {} and {}",
            cap(a, 80),
            cap(b, 80)
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::{Thread, ThreadStatus};
    use super::*;

    #[test]
    fn glob_match_covers_the_supported_grammar() {
        assert!(glob_match("src/**", "src/main.rs"));
        assert!(glob_match("src/**", "src/parse/deep/lexer.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/parse/lexer.rs"));
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "Cargo.lock"));
        assert!(glob_match("src/m?in.rs", "src/main.rs"));
        // Literal patterns double as directory prefixes.
        assert!(glob_match("src", "src/anything/below.rs"));
        assert!(!glob_match("src", "srcs/below.rs"));
    }

    #[test]
    fn overlap_is_approximate_but_sane() {
        // Same subtree: overlap.
        assert!(patterns_may_overlap("src/**", "src/parse/**"));
        assert!(patterns_may_overlap("src/parse/**", "src/**"));
        // Disjoint literal prefixes: no overlap.
        assert!(!patterns_may_overlap("src/parse/**", "src/lex/**"));
        assert!(!patterns_may_overlap("docs/**", "src/**"));
        // Exact file vs subtree containing it.
        assert!(patterns_may_overlap("Cargo.toml", "**"));
        assert!(!patterns_may_overlap("Cargo.toml", "src/**"));
        // Sibling-depth wildcards: assumed to co-match (documented
        // over-report).
        assert!(patterns_may_overlap("src/a*.rs", "src/b*.rs"));
        // A consumed non-literal pattern does not act as a dir prefix.
        assert!(!patterns_may_overlap("src/*.rs", "src/parse/**"));
        // A consumed LITERAL pattern does.
        assert!(patterns_may_overlap("src", "src/parse/**"));
    }

    #[test]
    fn scope_validation_refuses_escapes() {
        assert!(validate_scope(&["src/**".into()]).is_ok());
        assert!(validate_scope(&[]).is_err());
        assert!(validate_scope(&["/etc/passwd".into()]).is_err());
        assert!(validate_scope(&["../up/**".into()]).is_err());
        assert!(validate_scope(&["a//b".into()]).is_err());
        assert_eq!(
            validate_scope(&["./src/**".into()]).unwrap(),
            vec!["src/**".to_string()]
        );
    }

    fn lease(id: &str, thread: &str, goal: &str, scope: &[&str]) -> Lease {
        Lease {
            id: id.into(),
            thread_id: thread.into(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            goal: goal.into(),
            criteria: vec![],
            holder: "t".into(),
            ttl_ms: 60_000,
            last_heartbeat_ms: 1_000,
        }
    }

    fn live_thread(id: &str, lease_id: &str) -> Thread {
        Thread {
            id: id.into(),
            repo_id: "repo-x".into(),
            goal: "g".into(),
            head_stitch: None,
            lease_id: Some(lease_id.into()),
            status: ThreadStatus::Active,
            note: String::new(),
            approval_id: None,
            worktree: None,
            base_stitch: None,
        }
    }

    #[test]
    fn toe_steps_carry_both_goals_and_a_suggested_split() {
        let mut rs = RepoState::default();
        rs.leases.push(lease("lease-1", "thread-1", "refactor the parser", &["src/parse/**"]));
        rs.threads.push(live_thread("thread-1", "lease-1"));
        let candidate = lease("lease-2", "thread-2", "rename parse symbols", &["src/**"]);
        let steps = detect_toe_steps(&rs, &candidate, 2_000);
        assert_eq!(steps.len(), 1);
        let t = &steps[0];
        assert_eq!(t.goal_a, "rename parse symbols");
        assert_eq!(t.goal_b, "refactor the parser");
        assert_eq!(t.pattern_a, "src/**");
        assert_eq!(t.pattern_b, "src/parse/**");
        assert!(!t.suggested_split.is_empty());
        assert!(
            t.suggested_split.iter().any(|s| s.contains("src")),
            "split names the shared prefix: {:?}",
            t.suggested_split
        );
    }

    #[test]
    fn disjoint_scopes_and_dead_leases_step_on_no_toes() {
        let mut rs = RepoState::default();
        rs.leases.push(lease("lease-1", "thread-1", "docs", &["docs/**"]));
        rs.threads.push(live_thread("thread-1", "lease-1"));
        // Disjoint literal prefixes.
        let candidate = lease("lease-2", "thread-2", "src work", &["src/**"]);
        assert!(detect_toe_steps(&rs, &candidate, 2_000).is_empty());
        // Overlapping but EXPIRED lease: no warning.
        let candidate = lease("lease-3", "thread-3", "docs too", &["docs/**"]);
        assert!(detect_toe_steps(&rs, &candidate, 10_000_000).is_empty());
        // Overlapping but its thread is woven (not live): no warning.
        rs.threads[0].status = ThreadStatus::Woven;
        assert!(detect_toe_steps(&rs, &candidate, 2_000).is_empty());
    }

    #[test]
    fn distinct_literal_prefixes_get_the_writes_itself_split() {
        let s = suggest_split("src/parse/**", "src/lex/**");
        assert!(s.iter().any(|x| x.contains("src/parse/**")), "{s:?}");
        assert!(s.iter().any(|x| x.contains("src/lex/**")), "{s:?}");
    }
}
