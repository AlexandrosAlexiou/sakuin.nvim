//! ripgrep-style queries: `pattern -- flags`. Compiles to a grep matcher (the
//! source of truth for matching) plus a trigram prefilter for the index.
//!
//! Supported flags: -F -i -s -S -w -x -g GLOB -t TYPE -T TYPE (and their long
//! forms). The default is a regex with smart case.

use std::path::Path;

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use regex_syntax::hir::{Class, ClassUnicode, Hir, HirKind};

pub struct Query {
    pub matcher: RegexMatcher,
    pub prefilter: Prefilter,
    globs: Option<Override>,
    types: Option<Types>,
}

/// Boolean expression over literals; every match contains all trigrams of
/// each required literal.
#[derive(Debug, PartialEq)]
pub enum Prefilter {
    All,
    Literal(String),
    And(Vec<Prefilter>),
    Or(Vec<Prefilter>),
}

#[derive(Default, Clone, Copy)]
enum Case {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Default)]
struct Flags {
    fixed: bool,
    case: Case,
    word: bool,
    line: bool,
    globs: Vec<String>,
    types: Vec<String>,
    types_not: Vec<String>,
}

impl Query {
    /// `Ok(None)` for an empty pattern: it would match every line.
    pub fn parse(raw: &str, root: &Path) -> Result<Option<Query>, String> {
        let (pattern, flags) = split_flags(raw.trim());
        if pattern.is_empty() {
            return Ok(None);
        }
        let f = parse_flags(flags)?;

        let mut b = RegexMatcherBuilder::new();
        b.line_terminator(Some(b'\n'))
            .word(f.word)
            .whole_line(f.line)
            .fixed_strings(f.fixed);
        match f.case {
            Case::Smart => b.case_smart(true),
            Case::Insensitive => b.case_insensitive(true),
            Case::Sensitive => &mut b,
        };
        // A half-typed regex like `foo(` searches literally instead of erroring per keystroke.
        let (matcher, fixed) = match b.build(pattern) {
            Ok(m) => (m, f.fixed),
            Err(_) => (
                b.fixed_strings(true)
                    .build(pattern)
                    .map_err(|e| e.to_string())?,
                true,
            ),
        };

        let src = if fixed {
            regex_syntax::escape(pattern)
        } else {
            pattern.to_string()
        };
        let prefilter = regex_syntax::parse(&src)
            .map(|h| prefilter(&h))
            .unwrap_or(Prefilter::All);

        Ok(Some(Query {
            matcher,
            prefilter,
            globs: build_globs(&f.globs, root)?,
            types: build_types(&f.types, &f.types_not)?,
        }))
    }

    pub fn path_allowed(&self, rel_path: &str) -> bool {
        self.globs
            .as_ref()
            .is_none_or(|o| !o.matched(rel_path, false).is_ignore())
            && self
                .types
                .as_ref()
                .is_none_or(|t| !t.matched(rel_path, false).is_ignore())
    }
}

/// Splits on the last ` --` that is followed by nothing or by flags, so a
/// pattern can still contain ` -- text`.
fn split_flags(q: &str) -> (&str, &str) {
    q.rmatch_indices(" --")
        .map(|(i, _)| (&q[..i], &q[i + 3..]))
        .find(|(_, rest)| {
            rest.is_empty()
                || (rest.starts_with(' ')
                    && (rest.trim().is_empty() || rest.trim_start().starts_with('-')))
        })
        .map_or((q, ""), |(pat, rest)| (pat.trim_end(), rest.trim()))
}

fn parse_flags(s: &str) -> Result<Flags, String> {
    let mut f = Flags::default();
    let mut toks = s.split_whitespace();
    while let Some(tok) = toks.next() {
        if tok == "-" {
            continue; // still typing
        }
        if let Some(long) = tok.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let short = match name {
                "fixed-strings" => 'F',
                "ignore-case" => 'i',
                "case-sensitive" => 's',
                "smart-case" => 'S',
                "word-regexp" => 'w',
                "line-regexp" => 'x',
                "glob" => 'g',
                "type" => 't',
                "type-not" => 'T',
                _ => return Err(format!("unknown flag: {tok}")),
            };
            f.apply(short, value, &mut toks)?;
        } else if let Some(shorts) = tok.strip_prefix('-') {
            // Bundled like rg: `-Fw`, `-g*.rs`, `-trust`.
            for (i, c) in shorts.char_indices() {
                if matches!(c, 'g' | 't' | 'T') {
                    let attached = &shorts[i + 1..];
                    f.apply(c, Some(attached).filter(|a| !a.is_empty()), &mut toks)?;
                    break;
                }
                f.apply(c, None, &mut toks)?;
            }
        } else {
            return Err(format!("unexpected argument: {tok}"));
        }
    }
    Ok(f)
}

impl Flags {
    fn apply<'a>(
        &mut self,
        flag: char,
        value: Option<&'a str>,
        rest: &mut impl Iterator<Item = &'a str>,
    ) -> Result<(), String> {
        match flag {
            'F' => self.fixed = true,
            'i' => self.case = Case::Insensitive,
            's' => self.case = Case::Sensitive,
            'S' => self.case = Case::Smart,
            'w' => self.word = true,
            'x' => self.line = true,
            'g' | 't' | 'T' => {
                // A value-less flag at the end is still being typed: ignore it.
                let Some(v) = value.or_else(|| rest.next()) else {
                    return Ok(());
                };
                let v = v.trim_matches(|c| c == '\'' || c == '"').to_string();
                match flag {
                    'g' => self.globs.push(v),
                    't' => self.types.push(v),
                    _ => self.types_not.push(v),
                }
            }
            _ => return Err(format!("unknown flag: -{flag}")),
        }
        Ok(())
    }
}

fn build_globs(globs: &[String], root: &Path) -> Result<Option<Override>, String> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut b = OverrideBuilder::new(root);
    for g in globs {
        b.add(g).map_err(|e| format!("bad glob {g}: {e}"))?;
    }
    b.build().map(Some).map_err(|e| e.to_string())
}

fn build_types(select: &[String], negate: &[String]) -> Result<Option<Types>, String> {
    if select.is_empty() && negate.is_empty() {
        return Ok(None);
    }
    let mut b = TypesBuilder::new();
    b.add_defaults();
    for t in select {
        b.select(t);
    }
    for t in negate {
        b.negate(t);
    }
    b.build().map(Some).map_err(|e| e.to_string())
}

fn prefilter(h: &Hir) -> Prefilter {
    match h.kind() {
        HirKind::Concat(subs) => {
            // Adjacent literal pieces join into one run so trigrams span them.
            let mut parts = Vec::new();
            let mut run = String::new();
            for s in subs {
                match literal(s) {
                    Some(l) => run.push_str(&l),
                    None => {
                        parts.push(lit(std::mem::take(&mut run)));
                        parts.push(prefilter(s));
                    }
                }
            }
            parts.push(lit(run));
            and(parts)
        }
        HirKind::Alternation(subs) => or(subs.iter().map(prefilter).collect()),
        HirKind::Capture(c) => prefilter(&c.sub),
        HirKind::Repetition(r) if r.min > 0 => prefilter(&r.sub),
        _ => literal(h).map_or(Prefilter::All, lit),
    }
}

/// The exact text `h` matches (modulo ASCII case), if it is fixed.
fn literal(h: &Hir) -> Option<String> {
    match h.kind() {
        // Zero-width: neighbours stay adjacent in the text.
        HirKind::Empty | HirKind::Look(_) => Some(String::new()),
        HirKind::Literal(l) => std::str::from_utf8(&l.0).ok().map(str::to_string),
        HirKind::Class(Class::Unicode(c)) => case_variants_of_one_char(c).map(String::from),
        HirKind::Capture(c) => literal(&c.sub),
        HirKind::Concat(subs) => subs.iter().map(literal).collect(),
        _ => None,
    }
}

/// `(?i)k` parses as `[Kk\u{212A}]`; the index folds ASCII case, so that is
/// just `k` to the prefilter.
/// shortcut: non-ASCII case variants (KELVIN SIGN, long s, É vs é) can slip
/// the prefilter because the index lowercases ASCII only; upgrade by folding
/// Unicode in the trigram tokenizer (needs a reindex).
fn case_variants_of_one_char(c: &ClassUnicode) -> Option<char> {
    let members: Vec<char> = c.iter().flat_map(|r| r.start()..=r.end()).take(5).collect();
    if members.len() > 4 {
        return None;
    }
    let base = members.iter().find(|m| m.is_ascii())?.to_ascii_lowercase();
    members
        .iter()
        .all(|m| {
            m.to_lowercase().eq(base.to_lowercase()) || m.to_uppercase().eq(base.to_uppercase())
        })
        .then_some(base)
}

fn lit(s: String) -> Prefilter {
    if s.chars().count() >= 3 {
        Prefilter::Literal(s)
    } else {
        Prefilter::All
    }
}

fn and(parts: Vec<Prefilter>) -> Prefilter {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Prefilter::All => {}
            Prefilter::And(inner) => out.extend(inner),
            p => out.push(p),
        }
    }
    match out.len() {
        0 => Prefilter::All,
        1 => out.pop().unwrap(),
        _ => Prefilter::And(out),
    }
}

fn or(mut parts: Vec<Prefilter>) -> Prefilter {
    if parts.is_empty() || parts.contains(&Prefilter::All) {
        return Prefilter::All;
    }
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    Prefilter::Or(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grep_matcher::Matcher;
    use Prefilter::*;

    fn q(raw: &str) -> Query {
        Query::parse(raw, Path::new("/")).unwrap().unwrap()
    }
    fn pf(raw: &str) -> Prefilter {
        q(raw).prefilter
    }
    fn l(s: &str) -> Prefilter {
        Literal(s.into())
    }
    fn hits(raw: &str, text: &str) -> bool {
        q(raw).matcher.is_match(text.as_bytes()).unwrap()
    }

    #[test]
    fn split() {
        assert_eq!(split_flags("foo -- -F"), ("foo", "-F"));
        assert_eq!(split_flags("foo --"), ("foo", ""));
        assert_eq!(split_flags("foo -- "), ("foo", ""));
        assert_eq!(
            split_flags("foo -- --word-regexp -F"),
            ("foo", "--word-regexp -F")
        );
        assert_eq!(split_flags("a -- b -- -w"), ("a -- b", "-w"));
        assert_eq!(split_flags("i -- j"), ("i -- j", ""));
        assert_eq!(split_flags("foo --bar"), ("foo --bar", ""));
    }

    #[test]
    fn flags() {
        assert!(hits("a.c -- -F", "a.c") && !hits("a.c -- -F", "abc"));
        assert!(hits("foo", "FOO") && !hits("Foo", "foo")); // smart case
        assert!(hits("Foo -- -i", "foo") && !hits("foo -- -s", "FOO"));
        assert!(!hits("foo -- -w", "foobar") && hits("foo -- --word-regexp", "a foo b"));
        assert!(!hits("foo -- -x", "foo bar") && hits("foo -- -Fx", "foo"));
        assert!(Query::parse("x -- -z", Path::new("/")).is_err());
        assert!(Query::parse("x -- --nope", Path::new("/")).is_err());
        assert!(Query::parse("x -- -t nosuchtype", Path::new("/")).is_err());
        assert!(Query::parse("x -- -g", Path::new("/")).is_ok()); // still typing
        assert!(Query::parse("   ", Path::new("/")).unwrap().is_none());
    }

    #[test]
    fn invalid_regex_falls_back_to_literal() {
        assert!(hits("fn new(x", "fn new(x)"));
        assert_eq!(pf("fn new(x"), l("fn new(x"));
    }

    #[test]
    fn path_filters() {
        let g = q("x -- -g *.rs -g '!src/gen/**'");
        assert!(g.path_allowed("src/a.rs"));
        assert!(!g.path_allowed("src/a.cc") && !g.path_allowed("src/gen/a.rs"));
        let n = q("x -- -g !tests/**");
        assert!(n.path_allowed("src/a.rs") && !n.path_allowed("tests/a.rs"));
        let ts = q("x -- -g *.ts -g !*.test.ts");
        assert!(ts.path_allowed("web/a.ts") && !ts.path_allowed("web/a.test.ts"));
        assert!(!ts.path_allowed("web/a.js"));
        let t = q("x -- -trust -T cpp");
        assert!(t.path_allowed("a.rs") && !t.path_allowed("a.py"));
        assert!(!q("x -- -T cpp").path_allowed("a.cc") && q("x -- -T cpp").path_allowed("a.py"));
    }

    #[test]
    fn prefilters() {
        assert_eq!(pf("foo.*bar"), And(vec![l("foo"), l("bar")]));
        assert_eq!(pf("foo|barx"), Or(vec![l("foo"), l("barx")]));
        assert_eq!(pf("foo|b"), All);
        assert_eq!(pf(r"\w+_\d+"), All);
        assert_eq!(pf("ab"), All);
        assert_eq!(pf("(?i)hello"), l("hello"));
        assert_eq!(pf(r"\bfoo\b(bar)+"), And(vec![l("foo"), l("bar")]));
        assert_eq!(pf("ab(cd)"), l("abcd"));
        assert_eq!(pf("x?abc"), l("abc"));
        assert_eq!(pf("[ab]cde"), l("cde"));
        assert_eq!(pf("a.c -- -F"), l("a.c"));
    }
}
