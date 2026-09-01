//! What a Tab does: the word under the cursor, and what it becomes.
//!
//! Pure, like [`super::parser`] and for the same reason. The only thing
//! completion needs a server for is the LISTING; every decision made from
//! that listing (which candidates match, whether they share enough to be
//! worth inserting, how the result has to be quoted so the parser gives
//! it back) is decided here and tested without one.
//!
//! Three rules carry the module, and the first two are what the console
//! shipped without:
//!
//! - **A Tab that cannot extend the word LISTS.** A completion that
//!   silently does nothing is indistinguishable from a key that is not
//!   wired, which is exactly how it was reported.
//! - **The namespace comes from the VERB.** `put` takes a local path
//!   first and a remote one second; completing both against the server
//!   offers files the user has no way to upload.
//! - **What goes into the buffer must survive [`super::parser::tokenize`]
//!   unchanged.** A name with a space, a quote or a `[` is ordinary, and
//!   inserting it raw builds a line that means something else.

use super::parser::{self, ArgSpace};

/// The quoting the word under the cursor is sitting inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quote {
    /// Unquoted. Protection, if any, is by backslash.
    None,
    /// Inside `'...'`: literal, no escapes, so any character but `'`
    /// itself passes through untouched.
    Single,
    /// Inside `"..."`: groups, and a backslash still escapes.
    Double,
}

/// The word Tab is completing, located in the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSpan {
    /// Byte index where the replacement begins. This is the start of the
    /// whole word INCLUDING any opening quote, so the insert can change
    /// the quoting style if the name it found needs a different one.
    pub start: usize,
    /// The word as the parser would see it: quotes stripped, escapes
    /// applied. This, not the raw text, is what a prefix match runs
    /// against.
    pub text: String,
    /// The quoting in force at the cursor.
    pub quote: Quote,
    /// The words completed before this one, unescaped. `before[0]` is the
    /// verb when there is one; an empty vec means the cursor is on the
    /// verb itself.
    pub before: Vec<String>,
}

impl WordSpan {
    /// Which namespace this word completes against.
    ///
    /// Flags are skipped when counting operands, because `get -P foo`
    /// puts `foo` in the same place `get foo` does and a table indexed by
    /// raw word position would disagree.
    pub fn space(&self) -> Space {
        let Some((verb, args)) = self.before.split_first() else {
            return Space::Verb;
        };
        let operand = args.iter().filter(|w| !is_flag(w)).count() + 1;
        match parser::operand_space(verb, operand) {
            Some(ArgSpace::Remote) => Space::Remote,
            Some(ArgSpace::Local) => Space::Local,
            Some(ArgSpace::None) | None => Space::Nothing,
        }
    }
}

/// Where the candidates for a word come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// The first word: complete against the command vocabulary.
    Verb,
    Remote,
    Local,
    /// Nothing to complete (a mode, a umask, a verb that takes no
    /// operands, or one operand past what the verb accepts).
    Nothing,
}

/// Whether a word is a flag rather than an operand. Mirrors
/// [`super::parser`]'s own rule, including that a bare `-` is a filename.
fn is_flag(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|body| !body.is_empty())
}

/// Locate the word under the cursor.
///
/// Scans with [`super::parser::tokenize`]'s rules, but LENIENTLY: a quote
/// opened and not yet closed is the ordinary state of a line being typed,
/// so it yields the word being completed rather than an error. That
/// leniency is the whole reason this is not `tokenize` itself.
pub fn word_at(line: &str, cursor: usize) -> WordSpan {
    let cursor = cursor.min(line.len());
    let head = &line[..cursor];

    let mut before: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut start = cursor;
    let mut in_word = false;
    let mut quote = Quote::None;
    let mut escaped = false;

    for (i, c) in head.char_indices() {
        if escaped {
            text.push(c);
            escaped = false;
            continue;
        }
        match (quote, c) {
            // Whitespace ends a word only outside quotes; inside, it is
            // the character the quoting exists for.
            (Quote::None, ' ' | '\t') => {
                if in_word {
                    before.push(std::mem::take(&mut text));
                    in_word = false;
                }
            }
            (Quote::None | Quote::Double, '\\') => {
                if !in_word {
                    in_word = true;
                    start = i;
                }
                escaped = true;
            }
            (Quote::None, '"') => {
                if !in_word {
                    in_word = true;
                    start = i;
                }
                quote = Quote::Double;
            }
            (Quote::None, '\'') => {
                if !in_word {
                    in_word = true;
                    start = i;
                }
                quote = Quote::Single;
            }
            (Quote::Double, '"') | (Quote::Single, '\'') => quote = Quote::None,
            (_, other) => {
                if !in_word {
                    in_word = true;
                    start = i;
                }
                text.push(other);
            }
        }
    }

    if !in_word {
        // The cursor sits after a separator: the word is empty and begins
        // here. That is a question ("what is there?"), not a no-op, and
        // answering it is what makes a bare `get <Tab>` list a directory.
        start = cursor;
    }

    WordSpan {
        start,
        text,
        quote,
        before,
    }
}

/// A name the caller listed, and whether it invites another component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub is_dir: bool,
}

impl Candidate {
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
        }
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
        }
    }
}

/// What the console does with a Tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// Replace the word under the cursor with this text.
    Insert(String),
    /// Paint these names; the line itself does not change. Directories
    /// carry a trailing separator so the list says what each one is.
    List(Vec<String>),
    /// Nothing matched. Painting nothing is what every shell does: a bell
    /// or an error for a key people press speculatively is noise.
    Nothing,
}

/// Split a path being completed into the directory the caller must list
/// and the prefix to match inside it.
///
/// `None` for the directory means no separator was typed, which is NOT
/// the same as `Some("")`: the first completes against the working
/// directory, the second against the root, and conflating them replaced
/// a typed `/var` with a relative `var`.
pub fn split_path(word: &str, sep: char) -> (Option<&str>, &str) {
    match word.rfind(sep) {
        Some(i) => (Some(&word[..i]), &word[i + sep.len_utf8()..]),
        None => (None, word),
    }
}

/// The separator to rebuild a local path with.
///
/// Windows accepts both and users type both, so the one already in the
/// word wins; with nothing to go on, the platform's own. Getting this
/// wrong is not cosmetic: a path mixing the two still opens, but the
/// completion of its NEXT component splits on the wrong character and
/// lists the wrong directory.
pub fn local_sep(word: &str) -> char {
    match (word.contains('\\'), word.contains('/')) {
        (true, false) => '\\',
        (false, true) => '/',
        _ if cfg!(windows) => '\\',
        _ => '/',
    }
}

/// Decide what a Tab does, given what the caller listed.
///
/// The three outcomes are the reason this is a value rather than a paint:
/// extending the word, showing what is available, and doing nothing are
/// different answers, and the version that only knew the first of them
/// looked broken for every prefix shared by two files.
pub fn plan(
    prefix: &str,
    dir: Option<&str>,
    sep: char,
    quote: Quote,
    candidates: &[Candidate],
) -> Completion {
    let matches: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        // A completion that has to be asked for by name is not a
        // completion: dotfiles only appear once the user typed the dot.
        .filter(|c| prefix.starts_with('.') || !c.name.starts_with('.'))
        .collect();

    match matches.as_slice() {
        [] => Completion::Nothing,
        // One candidate completes fully, with the marker that says what
        // it is: a separator invites the next component, a space ends the
        // word.
        [only] => Completion::Insert(render_insert(
            &rebuild(dir, sep, &only.name),
            quote,
            sep,
            Some(only.is_dir),
        )),
        _ => {
            let common = common_prefix(matches.iter().map(|c| c.name.as_str())).unwrap_or_default();
            if common.len() > prefix.len() {
                // There is still something to add, so add it and say
                // nothing: the user has not asked twice yet.
                Completion::Insert(render_insert(
                    &rebuild(dir, sep, &common),
                    quote,
                    sep,
                    None,
                ))
            } else {
                // Nothing left to add. THIS is the case that made Tab
                // look dead: the common prefix is what the user already
                // typed, so the insert was a repaint of an identical
                // line. What they were asking for is the list.
                Completion::List(
                    matches
                        .iter()
                        .map(|c| {
                            if c.is_dir {
                                format!("{}{sep}", c.name)
                            } else {
                                c.name.clone()
                            }
                        })
                        .collect(),
                )
            }
        }
    }
}

/// Put a completed name back under the directory the user had typed.
///
/// `None` = no separator was typed, so the name stands alone. `Some("")`
/// = a leading separator and nothing else, so the name belongs under the
/// root; returning it bare there would turn an absolute path into a
/// relative one.
fn rebuild(dir: Option<&str>, sep: char, name: &str) -> String {
    match dir {
        None => name.to_string(),
        Some("") => format!("{sep}{name}"),
        Some(d) => format!("{d}{sep}{name}"),
    }
}

/// Render `path` so the parser gives it back verbatim.
///
/// `finished` is `None` while the word is still being narrowed, `Some`
/// once a single candidate settled it: `Some(true)` leaves the quoting
/// OPEN and appends a separator, because the next component belongs
/// inside the same quotes; `Some(false)` closes it and ends the word.
fn render_insert(path: &str, quote: Quote, sep: char, finished: Option<bool>) -> String {
    let style = style(path, quote);
    let mut out = match style {
        Quote::None => escape_bare(path),
        Quote::Single => format!("'{path}"),
        Quote::Double => format!("\"{}", escape_double(path)),
    };
    match finished {
        None => {}
        Some(true) => out.push(sep),
        Some(false) => {
            match style {
                Quote::None => {}
                Quote::Single => out.push('\''),
                Quote::Double => out.push('"'),
            }
            out.push(' ');
        }
    }
    out
}

/// Which quoting to write the path in.
///
/// A style the user already started is kept, because they are mid-word
/// and rewriting it under them would move the cursor somewhere they did
/// not put it. There is exactly one exception, and it is why the span
/// starts AT the opening quote rather than after it: single quotes are
/// literal, so they can carry any character except an apostrophe, and a
/// name holding one has to be rewritten in the other style or the line
/// stops parsing.
///
/// Choosing for an unquoted word turns on ONE character. A backslash
/// cannot be backslash-escaped into an unquoted word here, because the
/// tokenizer reads `\` as an escape everywhere, so `C:\Users` would come
/// back as `C:Users`: a Windows path in an unquoted word is destroyed by
/// the grammar itself. Single quotes have no such reading, so they are
/// the answer, and double quotes are the fallback for the one case
/// single quotes cannot express.
fn style(path: &str, started: Quote) -> Quote {
    match started {
        Quote::Double => Quote::Double,
        Quote::Single if path.contains('\'') => Quote::Double,
        Quote::Single => Quote::Single,
        Quote::None if !path.contains('\\') => Quote::None,
        Quote::None if path.contains('\'') => Quote::Double,
        Quote::None => Quote::Single,
    }
}

/// Backslash-escape what an unquoted word cannot carry raw.
///
/// The glob characters are in the set on purpose. A file literally named
/// `report[1].txt` is ordinary, and inserting it unescaped hands the
/// executor a PATTERN: the transfer then fails with "no matches found"
/// naming a file that is plainly there.
fn escape_bare(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, ' ' | '\t' | '"' | '\'' | '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Inside double quotes only the two characters the grammar reads there
/// need protecting; a space or a glob character is already covered by the
/// quoting itself.
fn escape_double(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The longest prefix every candidate shares, or `None` when there are no
/// candidates. Compares by CHARACTER, so a shared multi-byte prefix is
/// not cut mid-character into something that is not valid UTF-8.
pub fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> Option<String> {
    let first = names.next()?;
    let mut common: Vec<char> = first.chars().collect();
    for name in names {
        let mut shared = 0;
        for (a, b) in common.iter().zip(name.chars()) {
            if *a != b {
                break;
            }
            shared += 1;
        }
        common.truncate(shared);
        if common.is_empty() {
            break;
        }
    }
    Some(common.into_iter().collect())
}

/// The verbs matching `prefix`, for a Tab on the first word.
pub fn verb_candidates(prefix: &str) -> Vec<Candidate> {
    parser::VERBS
        .iter()
        .filter(|v| v.name.starts_with(prefix))
        .map(|v| Candidate::file(v.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: &str) -> WordSpan {
        word_at(line, line.len())
    }

    // --- locating the word ------------------------------------------

    #[test]
    fn the_word_is_what_follows_the_last_separator() {
        let w = at("get acc");
        assert_eq!(w.text, "acc");
        assert_eq!(w.start, 4);
        assert_eq!(w.before, vec!["get"]);
    }

    /// A trailing space is a question, not a no-op: the word is empty and
    /// begins at the cursor, which is what makes `get <Tab>` list.
    #[test]
    fn a_trailing_space_starts_an_empty_word() {
        let w = at("get ");
        assert_eq!(w.text, "");
        assert_eq!(w.start, 4);
        assert_eq!(w.before, vec!["get"]);
    }

    #[test]
    fn an_empty_line_is_completing_the_verb() {
        let w = at("");
        assert_eq!(w.before, Vec::<String>::new());
        assert_eq!(w.space(), Space::Verb);
    }

    /// The case the old comment claimed and the old code did not do: an
    /// open quote is the ordinary state of a line being typed, and the
    /// word is everything inside it.
    #[test]
    fn an_open_double_quote_holds_the_whole_word() {
        let w = at(r#"get "my fi"#);
        assert_eq!(w.text, "my fi");
        assert_eq!(w.quote, Quote::Double);
        // The replacement starts AT the quote, so a name needing a
        // different style can still be written.
        assert_eq!(w.start, 4);
    }

    #[test]
    fn an_open_single_quote_holds_the_whole_word() {
        let w = at("get 'my fi");
        assert_eq!(w.text, "my fi");
        assert_eq!(w.quote, Quote::Single);
    }

    #[test]
    fn a_closed_quote_leaves_the_word_unquoted() {
        let w = at(r#"get "my file" x"#);
        assert_eq!(w.text, "x");
        assert_eq!(w.quote, Quote::None);
        assert_eq!(w.before, vec!["get", "my file"]);
    }

    #[test]
    fn a_backslash_escaped_space_stays_inside_the_word() {
        let w = at(r"get my\ fi");
        assert_eq!(w.text, "my fi");
        assert_eq!(w.start, 4);
    }

    #[test]
    fn the_cursor_can_sit_before_the_end_of_the_line() {
        // `get ac|.log` -> completing `ac`, not `ac.log`.
        let w = word_at("get ac.log", 6);
        assert_eq!(w.text, "ac");
    }

    // --- which namespace --------------------------------------------

    /// The whole of the third report: `put` completes LOCAL files. The
    /// version that always listed the remote directory found nothing for
    /// a local name and painted nothing, so Tab read as unwired.
    #[test]
    fn put_completes_locally_and_get_remotely() {
        assert_eq!(at("get x").space(), Space::Remote);
        assert_eq!(at("put x").space(), Space::Local);
        assert_eq!(at("mput x").space(), Space::Local);
        assert_eq!(at("reput x").space(), Space::Local);
        assert_eq!(at("mget x").space(), Space::Remote);
    }

    /// The second operand crosses over: `get remote local`.
    #[test]
    fn the_second_operand_lives_on_the_other_side() {
        assert_eq!(at("get a.log b").space(), Space::Local);
        assert_eq!(at("put a.log b").space(), Space::Remote);
    }

    /// Flags do not count as operands, so a flag before the path does not
    /// shift it into the next column of the table.
    #[test]
    fn flags_do_not_shift_the_operand_position() {
        assert_eq!(at("get -P a").space(), Space::Remote);
        assert_eq!(at("get -P a.log b").space(), Space::Local);
    }

    #[test]
    fn local_verbs_complete_locally() {
        assert_eq!(at("lcd x").space(), Space::Local);
        assert_eq!(at("lls x").space(), Space::Local);
        assert_eq!(at("lmkdir x").space(), Space::Local);
    }

    #[test]
    fn chmod_does_not_complete_its_mode() {
        assert_eq!(at("chmod 6").space(), Space::Nothing);
        assert_eq!(at("chmod 644 f").space(), Space::Remote);
    }

    #[test]
    fn a_verb_with_no_operands_completes_nothing() {
        assert_eq!(at("pwd x").space(), Space::Nothing);
        assert_eq!(at("version x").space(), Space::Nothing);
    }

    #[test]
    fn an_operand_past_what_the_verb_takes_completes_nothing() {
        assert_eq!(at("cd a b").space(), Space::Nothing);
        assert_eq!(at("rename a b c").space(), Space::Nothing);
    }

    /// `rm` takes any number of remote paths, which the table expresses
    /// by its LAST entry repeating rather than by a special case.
    #[test]
    fn a_repeating_operand_keeps_its_namespace() {
        assert_eq!(at("rm a b c d").space(), Space::Remote);
    }

    #[test]
    fn an_unknown_verb_completes_nothing() {
        assert_eq!(at("frobnicate x").space(), Space::Nothing);
    }

    // --- splitting --------------------------------------------------

    #[test]
    fn a_path_splits_at_its_last_separator() {
        assert_eq!(split_path("var/lo", '/'), (Some("var"), "lo"));
        assert_eq!(split_path("lo", '/'), (None, "lo"));
        assert_eq!(split_path("/va", '/'), (Some(""), "va"));
        assert_eq!(split_path(r"C:\Users\wil", '\\'), (Some(r"C:\Users"), "wil"));
    }

    #[test]
    fn the_local_separator_follows_what_was_typed() {
        assert_eq!(local_sep(r"C:\Users\"), '\\');
        assert_eq!(local_sep("/tmp/"), '/');
    }

    // --- planning ---------------------------------------------------

    #[test]
    fn one_candidate_completes_and_ends_the_word() {
        let c = [Candidate::file("access.log")];
        assert_eq!(
            plan("acc", None, '/', Quote::None, &c),
            Completion::Insert("access.log ".into())
        );
    }

    #[test]
    fn one_directory_completes_and_invites_the_next_component() {
        let c = [Candidate::dir("logs")];
        assert_eq!(
            plan("lo", None, '/', Quote::None, &c),
            Completion::Insert("logs/".into())
        );
    }

    /// Several candidates that still share more than what was typed:
    /// extend, and say nothing yet.
    #[test]
    fn several_candidates_extend_to_the_common_prefix() {
        let c = [Candidate::file("access.log"), Candidate::file("access.old")];
        assert_eq!(
            plan("ac", None, '/', Quote::None, &c),
            Completion::Insert("access.".into())
        );
    }

    /// The reported bug, exactly: `ab` against `abcd.txt` and `abde.txt`
    /// shares nothing beyond `ab`, so the old code repainted an identical
    /// line and Tab looked dead. It must LIST.
    #[test]
    fn candidates_with_nothing_left_to_share_are_listed() {
        let c = [Candidate::file("abcd.txt"), Candidate::file("abde.txt")];
        assert_eq!(
            plan("ab", None, '/', Quote::None, &c),
            Completion::List(vec!["abcd.txt".into(), "abde.txt".into()])
        );
    }

    /// An empty prefix is the same question with nothing typed, and it
    /// has the same answer.
    #[test]
    fn an_empty_prefix_lists_the_directory() {
        let c = [Candidate::file("a"), Candidate::dir("b")];
        assert_eq!(
            plan("", None, '/', Quote::None, &c),
            Completion::List(vec!["a".into(), "b/".into()])
        );
    }

    #[test]
    fn nothing_matching_does_nothing() {
        let c = [Candidate::file("access.log")];
        assert_eq!(plan("zz", None, '/', Quote::None, &c), Completion::Nothing);
    }

    #[test]
    fn dotfiles_appear_only_once_the_dot_is_typed() {
        let c = [Candidate::file(".bashrc"), Candidate::file("bin")];
        assert_eq!(
            plan("", None, '/', Quote::None, &c),
            Completion::Insert("bin ".into()),
            "a dotfile counted as a candidate and blocked the only visible one"
        );
        assert_eq!(
            plan(".", None, '/', Quote::None, &c),
            Completion::Insert(".bashrc ".into())
        );
    }

    #[test]
    fn a_completion_keeps_the_directory_that_was_typed() {
        let c = [Candidate::dir("log")];
        assert_eq!(
            plan("lo", Some("/var"), '/', Quote::None, &c),
            Completion::Insert("/var/log/".into())
        );
    }

    /// A leading separator and nothing else is the ROOT, not "no
    /// directory": returning the name bare would turn the absolute path
    /// the user typed into a relative one.
    #[test]
    fn a_completion_at_the_root_stays_absolute() {
        let c = [Candidate::dir("var")];
        assert_eq!(
            plan("va", Some(""), '/', Quote::None, &c),
            Completion::Insert("/var/".into())
        );
    }

    // --- quoting ----------------------------------------------------

    /// The property every insert has to hold: what goes into the buffer
    /// comes back out of the pipeline as the name it stands for. Asserted
    /// through the REAL passes, because a hand-written expectation about
    /// the escaping would pass while the pipeline that reads it disagreed.
    fn round_trips(operand: &str, name: &str) {
        let words = parser::tokenize(&format!("get {operand}")).unwrap();
        assert_eq!(words.len(), 2, "{operand:?} did not stay one operand");
        assert_eq!(
            super::super::glob::unescape(&words[1]),
            name,
            "{operand:?} did not resolve back to the name"
        );
    }

    /// The half the old code had no answer for: a name with a space
    /// inserted raw produces a line the parser splits into two operands,
    /// so the transfer names a file nobody has.
    #[test]
    fn a_name_with_a_space_is_escaped_into_an_unquoted_word() {
        let c = [Candidate::file("my report.pdf")];
        let Completion::Insert(text) = plan("my", None, '/', Quote::None, &c) else {
            panic!("expected an insert");
        };
        assert_eq!(text, r"my\ report.pdf ");
        round_trips(&text, "my report.pdf");
    }

    /// Glob characters are protected for the same reason, and the
    /// assertion that matters is the one about `has_magic`: unescaped,
    /// the executor reads the name as a PATTERN and reports no matches
    /// for a file that is plainly there.
    #[test]
    fn glob_characters_in_a_name_are_escaped() {
        let c = [Candidate::file("report[1].txt")];
        let Completion::Insert(text) = plan("rep", None, '/', Quote::None, &c) else {
            panic!("expected an insert");
        };
        round_trips(&text, "report[1].txt");
        let operand = &parser::tokenize(&format!("get {text}")).unwrap()[1];
        assert!(
            !super::super::glob::has_magic(operand),
            "the completed name still reads as a pattern"
        );
    }

    /// A quote the user already opened is kept, and closed for them once
    /// a single candidate settled the word.
    #[test]
    fn an_open_double_quote_is_continued_and_closed() {
        let c = [Candidate::file("my report.pdf")];
        assert_eq!(
            plan("my", None, '/', Quote::Double, &c),
            Completion::Insert("\"my report.pdf\" ".into())
        );
    }

    /// A directory leaves the quote OPEN: the next component belongs
    /// inside the same quotes, and closing it would make the user delete
    /// the quote to carry on.
    #[test]
    fn a_quoted_directory_leaves_the_quote_open() {
        let c = [Candidate::dir("My Documents")];
        assert_eq!(
            plan("My", None, '/', Quote::Double, &c),
            Completion::Insert("\"My Documents/".into())
        );
    }

    #[test]
    fn a_single_quoted_word_stays_literal() {
        let c = [Candidate::file("a file.txt")];
        assert_eq!(
            plan("a", None, '/', Quote::Single, &c),
            Completion::Insert("'a file.txt' ".into())
        );
    }

    /// The one case that overrides the user's own quoting, and the reason
    /// the span starts at the opening quote: an apostrophe cannot be
    /// escaped inside single quotes, so keeping the style would emit a
    /// line that stops parsing.
    #[test]
    fn an_apostrophe_rewrites_single_quotes_into_double() {
        let c = [Candidate::file("it's here.txt")];
        let Completion::Insert(text) = plan("it", None, '/', Quote::Single, &c) else {
            panic!("expected an insert");
        };
        assert_eq!(text, "\"it's here.txt\" ");
        round_trips(&text, "it's here.txt");
    }

    /// A Windows path cannot go into an unquoted word at all: the
    /// tokenizer reads `\` as an escape everywhere, so `C:\Users` would
    /// come back as `C:Users`. Single quotes are literal and have no such
    /// reading.
    #[test]
    fn a_windows_path_is_single_quoted() {
        let c = [Candidate::dir("Documents")];
        let Completion::Insert(text) = plan("Doc", Some(r"C:\Users"), '\\', Quote::None, &c) else {
            panic!("expected an insert");
        };
        assert_eq!(text, r"'C:\Users\Documents\");
        // Closed by hand, the way the user would: it round-trips.
        round_trips(&format!("{text}'"), r"C:\Users\Documents\");
    }

    /// The one case single quotes cannot express: a backslash AND an
    /// apostrophe. Double quotes can, because a backslash escapes there.
    #[test]
    fn a_windows_path_with_an_apostrophe_falls_back_to_double_quotes() {
        let c = [Candidate::file("wil's notes.txt")];
        let Completion::Insert(text) = plan("wil", Some(r"C:\a"), '\\', Quote::None, &c) else {
            panic!("expected an insert");
        };
        round_trips(&text, r"C:\a\wil's notes.txt");
    }

    // --- verbs -------------------------------------------------------

    #[test]
    fn verbs_complete_from_the_command_table() {
        let c = verb_candidates("re");
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"reget"));
        assert!(names.contains(&"reput"));
        assert!(names.contains(&"rename"));
        assert!(!names.contains(&"get"));
    }

    /// A Tab on the verb with nothing typed offers the whole vocabulary
    /// rather than nothing, which is how a user finds out what a console
    /// they have never used can do.
    #[test]
    fn an_empty_verb_offers_everything() {
        assert_eq!(verb_candidates("").len(), parser::VERBS.len());
    }

    #[test]
    fn a_verb_that_settles_gets_a_trailing_space() {
        assert_eq!(
            plan("prog", None, '/', Quote::None, &verb_candidates("prog")),
            Completion::Insert("progress ".into())
        );
    }

    // --- common prefix ------------------------------------------------

    #[test]
    fn common_prefix_of_one_is_itself() {
        assert_eq!(
            common_prefix(["access.log"].into_iter()),
            Some("access.log".to_string())
        );
    }

    #[test]
    fn common_prefix_stops_at_the_first_difference() {
        assert_eq!(
            common_prefix(["access.log", "access.log.1", "access.old"].into_iter()),
            Some("access.".to_string())
        );
    }

    #[test]
    fn common_prefix_of_nothing_is_none() {
        assert_eq!(common_prefix(std::iter::empty()), None);
    }

    #[test]
    fn common_prefix_can_be_empty_when_nothing_is_shared() {
        assert_eq!(
            common_prefix(["alpha", "beta"].into_iter()),
            Some(String::new())
        );
    }

    /// Comparing by character rather than by byte is what keeps a shared
    /// multi-byte prefix from being cut in half, which would produce a
    /// completion that is not valid UTF-8 at all.
    #[test]
    fn common_prefix_does_not_cut_a_character_in_half() {
        assert_eq!(
            common_prefix(["文档a", "文档b"].into_iter()),
            Some("文档".to_string())
        );
    }

    /// A CJK name completes whole rather than to a truncated prefix, and
    /// the insert is valid UTF-8 either way.
    #[test]
    fn a_cjk_name_completes_whole() {
        let c = [Candidate::file("文档.txt")];
        assert_eq!(
            plan("文", None, '/', Quote::None, &c),
            Completion::Insert("文档.txt ".into())
        );
    }
}
