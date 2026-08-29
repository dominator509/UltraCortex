//! Facet glob matching with negation — SPEC-DERIVED-§A.5 (RouterScheduler.md),
//! SPEC-DERIVED-§4 (CURATOR_PAIR_PROTOCOL.md).
//!
//! Grammar: `facet_scope := <include_glob>+ (!<exclude_glob>)*`, whitespace
//! separated. A facet is allowed iff it matches at least one include glob and
//! no exclude glob. This is the protocol-level enforcement of P19 (Asymmetric
//! Visibility): the Warden literally cannot hydrate a Librarian rationale
//! blob because the Router rejects the request before dispatch.
//!
//! Wildcards: `**` matches any characters (including `/`), `*` matches any
//! characters except `/`, `?` matches a single non-`/` character. All other
//! characters (including `.`) match literally.
//!
//! Canonicalization ([GAP-CU-012]): tokens are trimmed, empty tokens dropped,
//! duplicate tokens deduped, order preserved otherwise.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetScope {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

impl FacetScope {
    /// Parse a facet-scope string. An empty/blank scope canonicalizes to a
    /// single `**` include (allow-all, no exclusions) — used for operator
    /// tokens; agent tokens are always issued with explicit exclusions.
    pub fn parse(s: &str) -> FacetScope {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        for raw in s.split_whitespace() {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(ex) = tok.strip_prefix('!') {
                if !ex.is_empty() && !excludes.iter().any(|e| e == ex) {
                    excludes.push(ex.to_string());
                }
            } else if !includes.iter().any(|i| i == tok) {
                includes.push(tok.to_string());
            }
        }
        if includes.is_empty() {
            includes.push("**".to_string());
        }
        FacetScope { includes, excludes }
    }

    /// Canonical string form (deterministic re-serialization).
    pub fn canonical(&self) -> String {
        let mut parts: Vec<String> = self.includes.clone();
        for e in &self.excludes {
            parts.push(format!("!{e}"));
        }
        parts.join(" ")
    }

    pub fn allows(&self, facet: &str) -> bool {
        if !self.includes.iter().any(|g| glob_match(g, facet)) {
            return false;
        }
        !self.excludes.iter().any(|g| glob_match(g, facet))
    }

    /// True when the facet is specifically blocked by an exclusion glob
    /// (as opposed to merely not included). This distinction feeds the
    /// `curator.rationale_access_denied` metric, which MUST be non-zero in
    /// production (RouterScheduler.md §A.5).
    pub fn excluded(&self, facet: &str) -> bool {
        self.excludes.iter().any(|g| glob_match(g, facet))
    }
}

/// Wildcard matcher. Iterative with single-star backtracking plus explicit
/// recursion for `**` (bounded by pattern length).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_from(&p, 0, &t, 0)
}

fn match_from(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                let double = pi + 1 < p.len() && p[pi + 1] == '*';
                if double {
                    // `**` — match any (possibly empty) sequence incl '/'.
                    let next = pi + 2;
                    if next >= p.len() {
                        return true;
                    }
                    let mut k = ti;
                    while k <= t.len() {
                        if match_from(p, next, t, k) {
                            return true;
                        }
                        k += 1;
                    }
                    return false;
                } else {
                    // `*` — any sequence not containing '/'.
                    let next = pi + 1;
                    if next >= p.len() {
                        return !t[ti..].contains(&'/');
                    }
                    let mut k = ti;
                    loop {
                        if match_from(p, next, t, k) {
                            return true;
                        }
                        if k >= t.len() || t[k] == '/' {
                            return false;
                        }
                        k += 1;
                    }
                }
            }
            '?' => {
                if ti >= t.len() || t[ti] == '/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_wildcards() {
        assert!(glob_match("fact/*", "fact/ABC"));
        assert!(!glob_match("fact/*", "fact/a/b"));
        assert!(glob_match("fact/**", "fact/a/b"));
        assert!(glob_match("**", "anything/at/all"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(glob_match("src/router/**", "src/router/mod.rs"));
        assert!(glob_match("librarian/output/*", "librarian/output/X"));
        assert!(!glob_match(
            "librarian/output/*",
            "librarian/output/X/rationale"
        ));
        assert!(glob_match(
            "librarian/output/*/rationale*",
            "librarian/output/X/rationale"
        ));
    }

    #[test]
    fn p19_warden_scope_blocks_rationale() {
        // The exact shape from CURATOR_PAIR_PROTOCOL.md §4.
        let scope = FacetScope::parse(
            "librarian/output/**  fact/**  decision/**  contract/**  anchor/** \
             !librarian/output/*/rationale**  !librarian/output/*/considered_alts** \
             !librarian/output/*/reasoning_trace**  !librarian/output/*/confidence_precise",
        );
        assert!(scope.allows("librarian/output/01ABC/public"));
        assert!(scope.allows("fact/01ABC"));
        assert!(!scope.allows("librarian/output/01ABC/rationale"));
        assert!(scope.excluded("librarian/output/01ABC/rationale"));
        assert!(!scope.allows("librarian/output/01ABC/reasoning_trace"));
        assert!(!scope.allows("librarian/output/01ABC/confidence_precise"));
        // Not included at all — denied but not "excluded" (different metric).
        assert!(!scope.allows("scratch/x"));
        assert!(!scope.excluded("scratch/x"));
    }

    #[test]
    fn adjudicator_scope_blocks_ledger() {
        let scope = FacetScope::parse(
            "librarian/output/** warden/judgment/** fact/** decision/** \
             !*/rationale** !**/reasoning_trace** !cross_check/**",
        );
        assert!(scope.allows("warden/judgment/01X/public"));
        assert!(!scope.allows("cross_check/01Y"));
        assert!(scope.excluded("cross_check/01Y"));
    }

    #[test]
    fn canonicalization() {
        let a = FacetScope::parse("  fact/**   !x/**  fact/** ");
        assert_eq!(a.includes, vec!["fact/**".to_string()]);
        assert_eq!(a.excludes, vec!["x/**".to_string()]);
        assert_eq!(a.canonical(), "fact/** !x/**");
        let empty = FacetScope::parse("   ");
        assert!(empty.allows("anything"));
    }
}
