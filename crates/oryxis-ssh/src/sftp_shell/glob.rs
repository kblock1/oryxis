//! Glob matching for the console's `mget` / `mput` / `rm` / `ls`.
//!
//! Written by hand rather than pulled from `globset`, which is excellent
//! and wrong for this: it exists to walk a filesystem, and everything
//! here matches against names a `list_dir` already returned. The whole
//! job is `*`, `?` and `[...]` over one path component, which is 60 lines
//! and no new dependency.
//!
//! It follows `sftp(1)`, which follows the shell: `*` does NOT cross a
//! `/`, so `old/*.gz` expands inside `old` and never further down. That
//! is why matching is per COMPONENT ([`matches_path`]) rather than one
//! pattern run over the whole string.

/// Whether `name` matches `pattern`, for ONE path component.
///
/// **No wildcard crosses a `/`, inside this function too.** The caller is
/// expected to split first ([`split_components`]), so in normal use a
/// separator never reaches here. Enforcing it anyway is deliberate: the
/// output of this feeds `rm`, and a caller that forgets to split would
/// otherwise turn `rm *` into a pattern that reaches down the tree. A
/// safety property that costs one comparison should not depend on every
/// call site remembering it.
///
/// A leading dot is not special here. `sftp(1)` lists dotfiles only with
/// `ls -a`, but that is the LISTING's rule; once the caller has decided
/// which entries exist, `*` matches what it is given. Keeping the two
/// rules apart is what lets `get .bashrc` and `ls -a *` both behave.
pub fn matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_from(&p, 0, &n, 0)
}

/// Backtracking matcher. Recursion is bounded by the pattern length and
/// the input is one filename, so the depth is measured in filename
/// characters, not in anything an attacker picks.
fn match_from(p: &[char], mut pi: usize, n: &[char], mut ni: usize) -> bool {
    // Position of the last `*` seen and where the input stood then, so a
    // failed branch can resume by letting the star eat one more
    // character. Iterative rather than recursive so a pathological
    // pattern (`***a***b`) cannot blow the stack.
    let mut star: Option<(usize, usize)> = None;

    while ni < n.len() {
        if pi < p.len() {
            match p[pi] {
                // A separator stops every wildcard: it can only be
                // matched by a literal `/` in the pattern.
                '*' if n[ni] != '/' => {
                    star = Some((pi, ni));
                    pi += 1;
                    continue;
                }
                '?' if n[ni] != '/' => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                '[' if n[ni] != '/' => {
                    if let Some((matched, next_pi)) = match_class(p, pi, n[ni]) {
                        if matched {
                            pi = next_pi;
                            ni += 1;
                            continue;
                        }
                    } else {
                        // Unterminated `[`: treat it as a literal, which
                        // is what the shell does and what keeps a typo
                        // from matching everything.
                        if p[pi] == n[ni] {
                            pi += 1;
                            ni += 1;
                            continue;
                        }
                    }
                }
                '\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == n[ni] {
                        pi += 2;
                        ni += 1;
                        continue;
                    }
                }
                c if c == n[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: back up to the last star and let it eat one more.
        // Unless the character it would eat is a separator, which no
        // wildcard may cross: then the star is spent and the match fails.
        match star {
            Some((sp, sn)) if n[sn] != '/' => {
                pi = sp + 1;
                ni = sn + 1;
                star = Some((sp, sn + 1));
            }
            _ => return false,
        }
    }

    // Input exhausted: the rest of the pattern must be stars.
    p[pi..].iter().all(|&c| c == '*')
}

/// Match a `[...]` class starting at `p[pi]` (which is `[`).
///
/// Returns `Some((matched, index just past the class))`, or `None` when
/// the class is unterminated.
fn match_class(p: &[char], pi: usize, c: char) -> Option<(bool, usize)> {
    let mut i = pi + 1;
    let negated = matches!(p.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        // A `]` in the first position is a literal, per POSIX.
        if p[i] == ']' && !first {
            let result = matched != negated;
            return Some((result, i + 1));
        }
        first = false;
        // A range, unless the `-` is last (then it is a literal).
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

/// Whether `pattern` holds any glob metacharacter. A path with none is
/// used verbatim, which is what makes `get file[1].txt` reach a file
/// literally named that when no glob matches it.
pub fn has_magic(pattern: &str) -> bool {
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// Drop one level of backslash escaping, turning a glob-escaped operand
/// back into the name it stands for.
///
/// The inverse of what [`super::parser::tokenize`] emits, and the last
/// step before an operand becomes a PATH. A trailing backslash has
/// nothing to escape and stands for itself, which is what keeps the
/// function total.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a path into components, keeping track of whether it was
/// absolute. Empty components (from `//` or a trailing `/`) are dropped,
/// matching how the caller joins them back.
pub fn split_components(path: &str) -> (bool, Vec<&str>) {
    let absolute = path.starts_with('/');
    let parts = path.split('/').filter(|s| !s.is_empty()).collect();
    (absolute, parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_matches_any_run() {
        assert!(matches("*", "anything"));
        assert!(matches("*.gz", "access.log.gz"));
        assert!(matches("access*", "access.log"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(!matches("*.gz", "access.log"));
    }

    #[test]
    fn a_star_matches_the_empty_run() {
        assert!(matches("a*", "a"));
        assert!(matches("*a*", "a"));
        assert!(matches("*", ""));
    }

    #[test]
    fn question_matches_exactly_one() {
        assert!(matches("?.txt", "a.txt"));
        assert!(!matches("?.txt", "ab.txt"));
        assert!(!matches("?", ""));
    }

    #[test]
    fn classes_match_sets_and_ranges() {
        assert!(matches("[abc].txt", "b.txt"));
        assert!(!matches("[abc].txt", "d.txt"));
        assert!(matches("log.[0-9]", "log.7"));
        assert!(!matches("log.[0-9]", "log.x"));
    }

    #[test]
    fn classes_negate_with_bang_or_caret() {
        assert!(matches("[!abc].txt", "d.txt"));
        assert!(!matches("[!abc].txt", "a.txt"));
        assert!(matches("[^0-9]", "x"));
        assert!(!matches("[^0-9]", "5"));
    }

    /// POSIX: a `]` right after the opening bracket is a literal, not the
    /// terminator. Getting this wrong makes `[]]` match nothing at all.
    #[test]
    fn a_leading_bracket_in_a_class_is_literal() {
        assert!(matches("[]]", "]"));
        assert!(matches("[]a]", "a"));
    }

    /// A trailing `-` is a literal rather than the start of a range.
    #[test]
    fn a_trailing_dash_in_a_class_is_literal() {
        assert!(matches("[a-]", "-"));
        assert!(matches("[a-]", "a"));
    }

    /// An unterminated class is a typo, and matching everything would be
    /// the worst possible answer for a pattern that feeds `rm`.
    #[test]
    fn an_unterminated_class_is_a_literal_bracket() {
        assert!(matches("[abc", "[abc"));
        assert!(!matches("[abc", "a"));
    }

    #[test]
    fn a_backslash_escapes_a_metacharacter() {
        assert!(matches(r"a\*b", "a*b"));
        assert!(!matches(r"a\*b", "axxb"));
        assert!(matches(r"\[x\]", "[x]"));
    }

    #[test]
    fn matching_is_literal_when_there_is_no_magic() {
        assert!(matches("file.txt", "file.txt"));
        assert!(!matches("file.txt", "file.txt.bak"));
    }

    /// The pathological pattern that a naive recursive matcher blows the
    /// stack (or the clock) on. Bounded here because the star backtrack
    /// is iterative.
    #[test]
    fn many_stars_do_not_explode() {
        let pattern = "*".repeat(40) + "b";
        let name = "a".repeat(200);
        assert!(!matches(&pattern, &name));
        assert!(matches(&pattern, &(name + "b")));
    }

    #[test]
    fn has_magic_sees_metacharacters_only() {
        assert!(has_magic("*.gz"));
        assert!(has_magic("f?le"));
        assert!(has_magic("[abc]"));
        assert!(!has_magic("plain.txt"));
        assert!(!has_magic(r"escaped\*"));
    }

    #[test]
    fn components_split_and_report_absoluteness() {
        assert_eq!(split_components("/var/log"), (true, vec!["var", "log"]));
        assert_eq!(split_components("old/*.gz"), (false, vec!["old", "*.gz"]));
        assert_eq!(split_components("/"), (true, vec![]));
        assert_eq!(split_components("a//b/"), (false, vec!["a", "b"]));
    }

    /// The rule that makes per-component matching necessary: a `*` never
    /// crosses a separator, so this must not match as one string.
    #[test]
    fn a_star_does_not_cross_a_separator_when_split() {
        let (_, parts) = split_components("old/*.gz");
        assert_eq!(parts.len(), 2);
        assert!(!matches(parts[1], "sub/x.gz"));
    }

    /// The same rule enforced INSIDE the matcher, not just by callers
    /// splitting first. This feeds `rm`, and a caller that forgot to
    /// split must not thereby get a pattern that reaches down the tree.
    #[test]
    fn no_wildcard_crosses_a_separator() {
        assert!(!matches("*", "a/b"));
        assert!(!matches("*.gz", "sub/x.gz"));
        assert!(!matches("a*c", "a/c"));
        assert!(!matches("?", "/"));
        assert!(!matches("[/]", "/"));
        // A literal separator in the pattern still matches one.
        assert!(matches("a/b", "a/b"));
        assert!(matches("*/*", "a/b"));
    }

    #[test]
    fn unicode_names_match_by_character() {
        assert!(matches("文*", "文档"));
        assert!(matches("?档", "文档"));
        assert!(!matches("?", "文档"));
    }
}
