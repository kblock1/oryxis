//! Turning a submitted line into a [`Command`].
//!
//! The grammar is `sftp(1)`'s, deliberately: the people who ask for this
//! console learned `get` and `lcd` from OpenSSH, on whatever machine they
//! were sitting at, and a dialect of our own would be a worse version of
//! something they already know.
//!
//! The tokenizer is written here rather than taken from `shell-words` for
//! the same reason [`super::glob`] is: the rules are small, they are
//! `sftp(1)`'s and not the POSIX shell's, and owning them means the
//! quoting a filename needs (`get "My Documents/report.pdf"`) is covered
//! by a test instead of by a dependency's interpretation.

/// A parsed console command. One variant per `sftp(1)` command we
/// implement; the flags each one accepts are fields rather than a shared
/// bag, so a flag that means nothing for a command cannot be silently
/// accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Change the remote directory. `None` = the session's home, which is
    /// what a bare `cd` does.
    Cd(Option<String>),
    /// Change the local directory. `None` = the local home.
    Lcd(Option<String>),
    /// Print the remote working directory.
    Pwd,
    /// Print the local working directory.
    Lpwd,
    Ls(LsOpts),
    Lls(LsOpts),
    /// Download. `local` is where it lands; `None` means "the basename,
    /// in the local working directory".
    Get {
        opts: XferOpts,
        remote: String,
        local: Option<String>,
    },
    /// Upload, mirroring [`Command::Get`].
    Put {
        opts: XferOpts,
        local: String,
        remote: Option<String>,
    },
    /// Remove remote files. Carries every operand because `rm a b c` is
    /// one command, and each may be a glob.
    Rm(Vec<String>),
    Mkdir(String),
    Lmkdir(String),
    Rmdir(String),
    Rename {
        from: String,
        to: String,
    },
    Chmod {
        mode: u32,
        paths: Vec<String>,
    },
    /// Toggle the transfer progress meter. `sftp(1)` takes no argument
    /// and reports the new state.
    Progress,
    /// `help` / `?`.
    Help,
    Version,
    /// `bye` / `quit` / `exit`.
    Quit,
}

/// Flags shared by `ls` and `lls`, which take the same set, plus the
/// optional path operand. Not `Copy`, because the path is owned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LsOpts {
    /// The path to list. `None` = the working directory.
    pub path: Option<String>,
    /// `-1`: one entry per line.
    pub one_per_line: bool,
    /// `-a`: include entries whose name starts with a dot.
    pub all: bool,
    /// `-f`: do not sort, list in the order the server answered.
    pub unsorted: bool,
    /// `-h`: with `-l`, sizes as 1.2K / 3.4M.
    pub human: bool,
    /// `-l`: long format.
    pub long: bool,
    /// `-n`: with `-l`, numeric uid/gid. Accepted and ignored, because
    /// numeric is all we can do: see the `longname` note in
    /// [`super::render`].
    pub numeric: bool,
    /// `-r`: reverse the sort.
    pub reverse: bool,
    /// `-S`: sort by size.
    pub by_size: bool,
    /// `-t`: sort by modification time.
    pub by_time: bool,
}

impl LsOpts {
    /// The path operand, kept next to the flags so callers pass one
    /// value. `None` = the working directory.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}

/// Flags shared by `get` and `put`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XferOpts {
    /// `-a`: resume a partial transfer.
    pub resume: bool,
    /// `-f`: fsync after writing.
    pub fsync: bool,
    /// `-p` / `-P`: preserve modification times and permissions.
    pub preserve: bool,
    /// `-r` / `-R`: recurse into directories.
    pub recursive: bool,
}

/// Why a line could not become a command. The messages these produce are
/// `sftp(1)`'s, so a user pasting from a tutorial sees what the tutorial
/// says they will see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Nothing but whitespace. Not an error to report; the caller just
    /// reprompts.
    Empty,
    /// First word is not a command we know.
    UnknownCommand(String),
    /// A quote was opened and never closed.
    UnterminatedQuote,
    /// The command needs operands it did not get.
    MissingOperand(&'static str),
    /// More operands than the command takes.
    TooManyOperands(&'static str),
    /// A flag the command does not accept.
    UnknownFlag { command: &'static str, flag: char },
    /// `chmod`'s mode did not parse as octal.
    BadMode(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => Ok(()),
            ParseError::UnknownCommand(c) => write!(f, "Invalid command: {c}"),
            ParseError::UnterminatedQuote => write!(f, "Unterminated quoted argument"),
            ParseError::MissingOperand(c) => {
                write!(f, "You must specify a path after the {c} command.")
            }
            ParseError::TooManyOperands(c) => write!(f, "Too many arguments for {c}."),
            ParseError::UnknownFlag { command, flag } => {
                write!(f, "{command}: unknown option -- {flag}")
            }
            ParseError::BadMode(m) => write!(f, "chmod: invalid mode: {m}"),
        }
    }
}

/// Which namespace an operand names.
///
/// This is `sftp(1)`'s own REMOTE / LOCAL / NOARGS column, which exists
/// there for exactly one consumer: completion. Without it a Tab has to
/// guess, and the guess is wrong for half the vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSpace {
    Remote,
    Local,
    /// Not a path: a mode, an owner id, a umask.
    None,
}

/// One row of the command vocabulary.
#[derive(Debug, Clone, Copy)]
pub struct Verb {
    pub name: &'static str,
    /// One entry per operand it accepts, in order. Empty means the verb
    /// takes none.
    pub operands: &'static [ArgSpace],
    /// Whether the LAST operand repeats. `rm a b c` is one command with
    /// three of them; `cd a b` is a mistake, and a completion that
    /// offered paths for the second would be helping the user build a
    /// line the parser is about to reject.
    pub variadic: bool,
}

/// Shorthand so the table below reads as a table.
const fn verb(name: &'static str, operands: &'static [ArgSpace], variadic: bool) -> Verb {
    Verb {
        name,
        operands,
        variadic,
    }
}

/// Every verb [`parse`] answers to, with the namespace of each operand.
///
/// **This table and [`parse`] are one change.** A verb in one and not the
/// other is either a command nothing can complete or a completion for a
/// command that does not exist, and both fail silently, which is why
/// `the_table_and_the_parser_know_the_same_verbs` compares them in both
/// directions.
pub const VERBS: &[Verb] = &[
    // Sorted by name, and `?` sorts first. It earns a row despite having
    // nothing to complete because the invariant below is "the table and
    // the parser know the same verbs", and an exception list is the thing
    // that eventually grows a second entry nobody checks.
    verb("?", &[], false),
    verb("bye", &[], false),
    verb("cd", &[ArgSpace::Remote], false),
    verb("chdir", &[ArgSpace::Remote], false),
    verb("chmod", &[ArgSpace::None, ArgSpace::Remote], true),
    verb("delete", &[ArgSpace::Remote], true),
    verb("dir", &[ArgSpace::Remote], false),
    verb("exit", &[], false),
    verb("get", &[ArgSpace::Remote, ArgSpace::Local], false),
    verb("help", &[], false),
    verb("lcd", &[ArgSpace::Local], false),
    verb("lchdir", &[ArgSpace::Local], false),
    verb("lls", &[ArgSpace::Local], false),
    verb("lmkdir", &[ArgSpace::Local], false),
    verb("lpwd", &[], false),
    verb("ls", &[ArgSpace::Remote], false),
    verb("mget", &[ArgSpace::Remote, ArgSpace::Local], false),
    verb("mkdir", &[ArgSpace::Remote], false),
    verb("mput", &[ArgSpace::Local, ArgSpace::Remote], false),
    verb("progress", &[], false),
    verb("put", &[ArgSpace::Local, ArgSpace::Remote], false),
    verb("pwd", &[], false),
    verb("quit", &[], false),
    verb("reget", &[ArgSpace::Remote, ArgSpace::Local], false),
    verb("rename", &[ArgSpace::Remote, ArgSpace::Remote], false),
    verb("reput", &[ArgSpace::Local, ArgSpace::Remote], false),
    verb("rm", &[ArgSpace::Remote], true),
    verb("rmdir", &[ArgSpace::Remote], false),
    verb("version", &[], false),
];

/// The namespace of `verb`'s `operand`-th operand, counting from 1.
///
/// `None` means there is nothing to complete: an unknown verb, a verb
/// that takes no operands, or an operand past what it accepts. All three
/// answer the same way because from a Tab's point of view they are the
/// same situation.
pub fn operand_space(verb: &str, operand: usize) -> Option<ArgSpace> {
    let lower = verb.to_ascii_lowercase();
    let row = VERBS.iter().find(|v| v.name == lower)?;
    if operand == 0 {
        return None;
    }
    match row.operands.get(operand - 1) {
        Some(space) => Some(*space),
        None if row.variadic => row.operands.last().copied(),
        None => None,
    }
}

/// Split a line into words, honouring quoting.
///
/// The rules, which are `sftp(1)`'s:
/// - a backslash escapes the next character anywhere;
/// - double quotes group, and a backslash inside them still escapes;
/// - single quotes group literally, with no escape inside;
/// - unquoted runs of spaces and tabs separate words.
///
/// An empty quoted string is a real, empty word: `put "" x` has two
/// operands, not one, which is what lets the caller report the mistake
/// instead of silently shifting arguments.
///
/// **The words come out GLOB-ESCAPED, not bare.** A character the user
/// quoted or escaped is a literal, and the only way to say so to the pass
/// that runs next is to hand it on still escaped: [`super::glob`] reads
/// `\[` as a literal bracket, and [`super::exec`] drops the escapes at
/// the moment an operand becomes a path. Unescaping here instead is what
/// made `get "report[1].txt"` fail with "no matches found" about a file
/// that was plainly there, because by the time the glob pass saw the name
/// the quoting that said "literal" was gone. `sftp(1)` does the same
/// thing in its own tokenizer, and for the same reason.
pub fn tokenize(line: &str) -> Result<Vec<String>, ParseError> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has_word = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            '\\' => match chars.next() {
                Some(next) => {
                    push_literal(&mut cur, next);
                    has_word = true;
                }
                None => return Err(ParseError::UnterminatedQuote),
            },
            '"' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(next) => push_literal(&mut cur, next),
                            None => return Err(ParseError::UnterminatedQuote),
                        },
                        Some(other) => push_literal(&mut cur, other),
                        None => return Err(ParseError::UnterminatedQuote),
                    }
                }
            }
            '\'' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(other) => push_literal(&mut cur, other),
                        None => return Err(ParseError::UnterminatedQuote),
                    }
                }
            }
            other => {
                // Unquoted, so a metacharacter here IS a wildcard and
                // travels bare. That asymmetry is the whole grammar:
                // `get *.gz` globs and `get "*.gz"` does not.
                cur.push(other);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(cur);
    }
    Ok(words)
}

/// Append a character the user asked to be taken literally.
///
/// The backslash is kept for the glob metacharacters AND for the
/// backslash itself. Keeping it for `\` is not tidiness: without it a
/// literal backslash and an escape become the same byte, and the next
/// pass cannot tell `a\*b` (a name with a star in it) from `a\b` followed
/// by a wildcard.
fn push_literal(cur: &mut String, c: char) {
    if matches!(c, '\\' | '*' | '?' | '[' | ']') {
        cur.push('\\');
    }
    cur.push(c);
}

/// Parse one submitted line.
pub fn parse(line: &str) -> Result<Command, ParseError> {
    let words = tokenize(line)?;
    let Some((verb, rest)) = words.split_first() else {
        return Err(ParseError::Empty);
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();

    match verb.to_ascii_lowercase().as_str() {
        "cd" | "chdir" => Ok(Command::Cd(one_optional(&rest, "cd")?)),
        "lcd" | "lchdir" => Ok(Command::Lcd(one_optional(&rest, "lcd")?)),
        "pwd" => Ok(Command::Pwd),
        "lpwd" => Ok(Command::Lpwd),
        "ls" | "dir" => Ok(Command::Ls(parse_ls(&rest, "ls")?)),
        "lls" => Ok(Command::Lls(parse_ls(&rest, "lls")?)),
        "get" | "reget" | "mget" => {
            let mut opts = XferOpts::default();
            // `reget` IS `get -a`, and `mget` is `get` whose operand is
            // expected to be a glob. Both are aliases in `sftp(1)`, so
            // they collapse here rather than growing variants that would
            // have to be handled identically everywhere downstream.
            if verb.eq_ignore_ascii_case("reget") {
                opts.resume = true;
            }
            let (opts, operands) = parse_xfer_flags(&rest, opts, "get")?;
            let mut it = operands.into_iter();
            let remote = it.next().ok_or(ParseError::MissingOperand("get"))?;
            let local = it.next();
            if it.next().is_some() {
                return Err(ParseError::TooManyOperands("get"));
            }
            Ok(Command::Get {
                opts,
                remote,
                local,
            })
        }
        "put" | "reput" | "mput" => {
            let mut opts = XferOpts::default();
            if verb.eq_ignore_ascii_case("reput") {
                opts.resume = true;
            }
            let (opts, operands) = parse_xfer_flags(&rest, opts, "put")?;
            let mut it = operands.into_iter();
            let local = it.next().ok_or(ParseError::MissingOperand("put"))?;
            let remote = it.next();
            if it.next().is_some() {
                return Err(ParseError::TooManyOperands("put"));
            }
            Ok(Command::Put {
                opts,
                local,
                remote,
            })
        }
        "rm" | "delete" => {
            if rest.is_empty() {
                return Err(ParseError::MissingOperand("rm"));
            }
            Ok(Command::Rm(rest.iter().map(|s| s.to_string()).collect()))
        }
        "mkdir" => Ok(Command::Mkdir(exactly_one(&rest, "mkdir")?)),
        "lmkdir" => Ok(Command::Lmkdir(exactly_one(&rest, "lmkdir")?)),
        "rmdir" => Ok(Command::Rmdir(exactly_one(&rest, "rmdir")?)),
        "rename" => {
            if rest.len() < 2 {
                return Err(ParseError::MissingOperand("rename"));
            }
            if rest.len() > 2 {
                return Err(ParseError::TooManyOperands("rename"));
            }
            Ok(Command::Rename {
                from: rest[0].to_string(),
                to: rest[1].to_string(),
            })
        }
        "chmod" => {
            if rest.len() < 2 {
                return Err(ParseError::MissingOperand("chmod"));
            }
            let mode = u32::from_str_radix(rest[0], 8)
                .map_err(|_| ParseError::BadMode(rest[0].to_string()))?;
            // A mode is at most four octal digits. Anything wider is a
            // typo that would otherwise be masked into something valid.
            if mode > 0o7777 {
                return Err(ParseError::BadMode(rest[0].to_string()));
            }
            Ok(Command::Chmod {
                mode,
                paths: rest[1..].iter().map(|s| s.to_string()).collect(),
            })
        }
        "progress" => Ok(Command::Progress),
        "help" | "?" => Ok(Command::Help),
        "version" => Ok(Command::Version),
        "bye" | "quit" | "exit" => Ok(Command::Quit),
        other => Err(ParseError::UnknownCommand(other.to_string())),
    }
}

/// A command taking zero or one operand.
fn one_optional(rest: &[&str], cmd: &'static str) -> Result<Option<String>, ParseError> {
    match rest.len() {
        0 => Ok(None),
        1 => Ok(Some(rest[0].to_string())),
        _ => Err(ParseError::TooManyOperands(cmd)),
    }
}

/// A command taking exactly one operand.
fn exactly_one(rest: &[&str], cmd: &'static str) -> Result<String, ParseError> {
    match rest.len() {
        0 => Err(ParseError::MissingOperand(cmd)),
        1 => Ok(rest[0].to_string()),
        _ => Err(ParseError::TooManyOperands(cmd)),
    }
}

fn parse_ls(rest: &[&str], cmd: &'static str) -> Result<LsOpts, ParseError> {
    let mut opts = LsOpts::default();
    let mut path = None;
    for word in rest {
        if let Some(flags) = flag_body(word) {
            for f in flags.chars() {
                match f {
                    '1' => opts.one_per_line = true,
                    'a' => opts.all = true,
                    'f' => opts.unsorted = true,
                    'h' => opts.human = true,
                    'l' => opts.long = true,
                    'n' => {
                        opts.numeric = true;
                        opts.long = true;
                    }
                    'r' => opts.reverse = true,
                    'S' => opts.by_size = true,
                    't' => opts.by_time = true,
                    other => {
                        return Err(ParseError::UnknownFlag {
                            command: cmd,
                            flag: other,
                        });
                    }
                }
            }
        } else if path.is_none() {
            path = Some(word.to_string());
        } else {
            return Err(ParseError::TooManyOperands(cmd));
        }
    }
    opts.path = path;
    Ok(opts)
}

fn parse_xfer_flags(
    rest: &[&str],
    mut opts: XferOpts,
    cmd: &'static str,
) -> Result<(XferOpts, Vec<String>), ParseError> {
    let mut operands = Vec::new();
    for word in rest {
        if let Some(flags) = flag_body(word) {
            for f in flags.chars() {
                match f {
                    'a' => opts.resume = true,
                    'f' => opts.fsync = true,
                    'p' | 'P' => opts.preserve = true,
                    'r' | 'R' => opts.recursive = true,
                    other => {
                        return Err(ParseError::UnknownFlag {
                            command: cmd,
                            flag: other,
                        });
                    }
                }
            }
        } else {
            operands.push(word.to_string());
        }
    }
    Ok((opts, operands))
}

/// The flag letters of `word`, or `None` when it is an operand.
///
/// A bare `-` is an operand, not an empty flag group: it is a legal
/// filename and treating it as a flag would make it unreachable.
fn flag_body(word: &str) -> Option<&str> {
    let body = word.strip_prefix('-')?;
    if body.is_empty() { None } else { Some(body) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse(line).unwrap_or_else(|e| panic!("{line:?} should parse, got {e:?}"))
    }

    // --- tokenizer --------------------------------------------------

    #[test]
    fn plain_words_split_on_whitespace() {
        assert_eq!(tokenize("get a b").unwrap(), vec!["get", "a", "b"]);
        assert_eq!(tokenize("  ls   -l  ").unwrap(), vec!["ls", "-l"]);
        assert_eq!(tokenize("").unwrap(), Vec::<String>::new());
    }

    /// The reason quoting exists at all here: a filename with a space is
    /// ordinary, and splitting it would reach the wrong file.
    #[test]
    fn double_quotes_group_a_filename_with_spaces() {
        assert_eq!(
            tokenize(r#"get "My Documents/report.pdf""#).unwrap(),
            vec!["get", "My Documents/report.pdf"]
        );
    }

    #[test]
    fn single_quotes_group_literally() {
        assert_eq!(
            tokenize(r#"get 'a "b" c'"#).unwrap(),
            vec!["get", r#"a "b" c"#]
        );
    }

    #[test]
    fn a_backslash_escapes_a_space_and_a_quote() {
        assert_eq!(tokenize(r"get my\ file").unwrap(), vec!["get", "my file"]);
        assert_eq!(tokenize(r#"get \"x"#).unwrap(), vec!["get", "\"x"]);
    }

    #[test]
    fn a_backslash_inside_double_quotes_still_escapes() {
        assert_eq!(tokenize(r#"get "a\"b""#).unwrap(), vec!["get", "a\"b"]);
    }

    #[test]
    fn an_empty_quoted_string_is_a_real_word() {
        assert_eq!(tokenize(r#"put "" x"#).unwrap(), vec!["put", "", "x"]);
    }

    /// The asymmetry the whole grammar rests on: unquoted, a
    /// metacharacter is a wildcard and travels bare; quoted, it is a
    /// literal and travels still escaped, so the glob pass downstream can
    /// tell which one it was looking at.
    #[test]
    fn a_quoted_glob_character_stays_escaped_and_an_unquoted_one_does_not() {
        assert_eq!(tokenize("get *.gz").unwrap(), vec!["get", "*.gz"]);
        assert_eq!(tokenize(r#"get "*.gz""#).unwrap(), vec!["get", r"\*.gz"]);
        assert_eq!(tokenize("get '*.gz'").unwrap(), vec!["get", r"\*.gz"]);
        assert_eq!(tokenize(r"get \*.gz").unwrap(), vec!["get", r"\*.gz"]);
    }

    /// The bug that made this the tokenizer's job: `report[1].txt` is an
    /// ordinary filename, and a version that unescaped here handed the
    /// glob pass a bracket it could only read as a class, so the transfer
    /// reported "no matches found" about a file that was plainly there.
    #[test]
    fn a_quoted_bracket_is_not_a_pattern() {
        let words = tokenize(r#"get "report[1].txt""#).unwrap();
        assert_eq!(words[1], r"report\[1\].txt");
        assert!(!super::super::glob::has_magic(&words[1]));
        assert_eq!(
            super::super::glob::unescape(&words[1]),
            "report[1].txt",
            "the escapes did not come back off"
        );
    }

    /// A literal backslash keeps its own escape, which is what stops it
    /// from being confused with the escape of the character after it.
    #[test]
    fn a_quoted_backslash_survives_next_to_a_quoted_wildcard() {
        let words = tokenize(r"get 'a\*b'").unwrap();
        assert!(!super::super::glob::has_magic(&words[1]));
        assert_eq!(super::super::glob::unescape(&words[1]), r"a\*b");
    }

    #[test]
    fn an_unterminated_quote_is_an_error() {
        assert_eq!(tokenize(r#"get "abc"#), Err(ParseError::UnterminatedQuote));
        assert_eq!(tokenize("get 'abc"), Err(ParseError::UnterminatedQuote));
        assert_eq!(tokenize(r"get abc\"), Err(ParseError::UnterminatedQuote));
    }

    // --- commands ---------------------------------------------------

    #[test]
    fn an_empty_line_parses_as_empty() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn navigation_commands() {
        assert_eq!(ok("cd /var/log"), Command::Cd(Some("/var/log".into())));
        assert_eq!(ok("cd"), Command::Cd(None));
        assert_eq!(
            ok("lcd ~/Downloads"),
            Command::Lcd(Some("~/Downloads".into()))
        );
        assert_eq!(ok("pwd"), Command::Pwd);
        assert_eq!(ok("lpwd"), Command::Lpwd);
    }

    /// The vocabulary lives in two places that nothing compiles together:
    /// the [`VERBS`] table, which completion reads, and the match arms in
    /// [`parse`]. A verb in one and not the other is either a command
    /// nothing can complete or a completion for a command that does not
    /// exist, and both fail silently. So they are compared in BOTH
    /// directions.
    #[test]
    fn the_table_and_the_parser_know_the_same_verbs() {
        for row in VERBS {
            assert!(
                !matches!(parse(row.name), Err(ParseError::UnknownCommand(_))),
                "{} is in VERBS but the parser rejects it",
                row.name
            );
        }
        // The other direction has to be spelled out: a `match` cannot be
        // iterated, and a table that silently lost a row is exactly what
        // this exists to catch.
        const PARSED: &[&str] = &[
            "cd", "chdir", "lcd", "lchdir", "pwd", "lpwd", "ls", "dir", "lls", "get", "reget",
            "mget", "put", "reput", "mput", "rm", "delete", "mkdir", "lmkdir", "rmdir", "rename",
            "chmod", "progress", "help", "?", "version", "bye", "quit", "exit",
        ];
        for name in PARSED {
            assert!(
                VERBS.iter().any(|v| v.name == *name),
                "the parser accepts {name} but VERBS has no row for it"
            );
        }
        assert_eq!(
            PARSED.len(),
            VERBS.len(),
            "VERBS has a row the parser does not accept"
        );
    }

    /// The table is read by hand far more often than it is searched, so
    /// it stays sorted.
    #[test]
    fn the_verb_table_is_sorted() {
        assert!(VERBS.windows(2).all(|w| w[0].name < w[1].name));
    }

    #[test]
    fn operand_namespaces_follow_the_verb() {
        assert_eq!(operand_space("get", 1), Some(ArgSpace::Remote));
        assert_eq!(operand_space("get", 2), Some(ArgSpace::Local));
        assert_eq!(operand_space("put", 1), Some(ArgSpace::Local));
        assert_eq!(operand_space("put", 2), Some(ArgSpace::Remote));
        assert_eq!(operand_space("PUT", 1), Some(ArgSpace::Local));
        assert_eq!(operand_space("chmod", 1), Some(ArgSpace::None));
        assert_eq!(operand_space("chmod", 2), Some(ArgSpace::Remote));
    }

    /// Only a variadic verb keeps offering paths past its last declared
    /// operand. `cd a b` is a mistake the parser will reject, and
    /// completing the second operand would be helping build it.
    #[test]
    fn only_a_variadic_verb_repeats_its_last_operand() {
        assert_eq!(operand_space("rm", 4), Some(ArgSpace::Remote));
        assert_eq!(operand_space("chmod", 5), Some(ArgSpace::Remote));
        assert_eq!(operand_space("cd", 2), None);
        assert_eq!(operand_space("get", 3), None);
        assert_eq!(operand_space("pwd", 1), None);
        assert_eq!(operand_space("frobnicate", 1), None);
    }

    #[test]
    fn chdir_and_lchdir_are_the_aliases_sftp_ships() {
        assert_eq!(ok("chdir /tmp"), Command::Cd(Some("/tmp".into())));
        assert_eq!(ok("lchdir /tmp"), Command::Lcd(Some("/tmp".into())));
    }

    #[test]
    fn command_names_are_case_insensitive() {
        assert_eq!(ok("PWD"), Command::Pwd);
        assert_eq!(ok("Ls"), Command::Ls(LsOpts::default()));
    }

    #[test]
    fn ls_flags_accumulate_and_can_be_bundled() {
        let Command::Ls(opts) = ok("ls -lah /tmp") else {
            panic!("not an ls");
        };
        assert!(opts.long && opts.all && opts.human);
        assert_eq!(opts.path(), Some("/tmp"));
    }

    #[test]
    fn ls_flags_can_be_separate_words() {
        let Command::Ls(opts) = ok("ls -l -a") else {
            panic!("not an ls");
        };
        assert!(opts.long && opts.all);
        assert_eq!(opts.path(), None);
    }

    /// `-n` implies `-l` in `sftp(1)`: numeric ids only mean something in
    /// the long format.
    #[test]
    fn ls_numeric_implies_long() {
        let Command::Ls(opts) = ok("ls -n") else {
            panic!("not an ls");
        };
        assert!(opts.numeric && opts.long);
    }

    #[test]
    fn an_unknown_flag_names_itself() {
        assert_eq!(
            parse("ls -z"),
            Err(ParseError::UnknownFlag {
                command: "ls",
                flag: 'z'
            })
        );
    }

    #[test]
    fn get_takes_one_or_two_operands() {
        assert_eq!(
            ok("get access.log"),
            Command::Get {
                opts: XferOpts::default(),
                remote: "access.log".into(),
                local: None
            }
        );
        assert_eq!(
            ok("get access.log /tmp/a.log"),
            Command::Get {
                opts: XferOpts::default(),
                remote: "access.log".into(),
                local: Some("/tmp/a.log".into())
            }
        );
        assert_eq!(parse("get"), Err(ParseError::MissingOperand("get")));
        assert_eq!(parse("get a b c"), Err(ParseError::TooManyOperands("get")));
    }

    #[test]
    fn transfer_flags_parse_in_either_case() {
        let Command::Get { opts, .. } = ok("get -rP x") else {
            panic!("not a get");
        };
        assert!(opts.recursive && opts.preserve);
        let Command::Put { opts, .. } = ok("put -Rp x") else {
            panic!("not a put");
        };
        assert!(opts.recursive && opts.preserve);
    }

    /// `reget` and `reput` are `-a` spelled as a verb, and collapsing
    /// them here is what keeps the resume path single downstream.
    #[test]
    fn reget_and_reput_are_resume() {
        let Command::Get { opts, .. } = ok("reget big.iso") else {
            panic!("not a get");
        };
        assert!(opts.resume);
        let Command::Put { opts, .. } = ok("reput big.iso") else {
            panic!("not a put");
        };
        assert!(opts.resume);
    }

    /// `mget` / `mput` are `get` / `put` whose operand happens to be a
    /// glob; the expansion is the executor's job, not the parser's.
    #[test]
    fn mget_and_mput_are_get_and_put() {
        assert_eq!(
            ok("mget *.gz"),
            Command::Get {
                opts: XferOpts::default(),
                remote: "*.gz".into(),
                local: None
            }
        );
        assert!(matches!(ok("mput *.txt"), Command::Put { .. }));
    }

    #[test]
    fn rm_takes_every_operand() {
        assert_eq!(
            ok("rm a b *.tmp"),
            Command::Rm(vec!["a".into(), "b".into(), "*.tmp".into()])
        );
        assert_eq!(parse("rm"), Err(ParseError::MissingOperand("rm")));
    }

    #[test]
    fn rename_needs_exactly_two() {
        assert_eq!(
            ok("rename a b"),
            Command::Rename {
                from: "a".into(),
                to: "b".into()
            }
        );
        assert_eq!(parse("rename a"), Err(ParseError::MissingOperand("rename")));
        assert_eq!(
            parse("rename a b c"),
            Err(ParseError::TooManyOperands("rename"))
        );
    }

    #[test]
    fn chmod_parses_an_octal_mode() {
        assert_eq!(
            ok("chmod 644 f"),
            Command::Chmod {
                mode: 0o644,
                paths: vec!["f".into()]
            }
        );
        assert_eq!(
            ok("chmod 4755 a b"),
            Command::Chmod {
                mode: 0o4755,
                paths: vec!["a".into(), "b".into()]
            }
        );
    }

    /// A decimal-looking mode is the classic mistake, and `999` is not
    /// octal at all. Reporting beats silently masking it into something
    /// that changes permissions the user did not ask for.
    #[test]
    fn chmod_rejects_a_mode_that_is_not_octal() {
        assert_eq!(parse("chmod 999 f"), Err(ParseError::BadMode("999".into())));
        assert_eq!(parse("chmod rwx f"), Err(ParseError::BadMode("rwx".into())));
        assert_eq!(
            parse("chmod 77777 f"),
            Err(ParseError::BadMode("77777".into()))
        );
        assert_eq!(parse("chmod 644"), Err(ParseError::MissingOperand("chmod")));
    }

    #[test]
    fn the_exit_family_is_one_command() {
        assert_eq!(ok("bye"), Command::Quit);
        assert_eq!(ok("quit"), Command::Quit);
        assert_eq!(ok("exit"), Command::Quit);
    }

    #[test]
    fn help_answers_to_both_spellings() {
        assert_eq!(ok("help"), Command::Help);
        assert_eq!(ok("?"), Command::Help);
    }

    #[test]
    fn an_unknown_command_names_itself() {
        assert_eq!(
            parse("frobnicate x"),
            Err(ParseError::UnknownCommand("frobnicate".into()))
        );
    }

    /// A file literally named `-` has to stay reachable, so a bare dash
    /// is an operand rather than an empty flag group.
    #[test]
    fn a_bare_dash_is_an_operand() {
        assert_eq!(
            ok("get -"),
            Command::Get {
                opts: XferOpts::default(),
                remote: "-".into(),
                local: None
            }
        );
    }

    /// A quoted operand that looks like a flag is an operand: quoting is
    /// how the user says so.
    #[test]
    fn a_quoted_dash_word_is_still_an_operand() {
        // The tokenizer strips the quotes, so this documents the known
        // limit: quoting does not survive into flag detection, exactly as
        // in `sftp(1)`. `./-l` is the portable way to name such a file.
        assert_eq!(
            ok("get ./-l"),
            Command::Get {
                opts: XferOpts::default(),
                remote: "./-l".into(),
                local: None
            }
        );
    }

    #[test]
    fn quoted_paths_survive_into_the_command() {
        assert_eq!(
            ok(r#"cd "/var/my logs""#),
            Command::Cd(Some("/var/my logs".into()))
        );
    }
}
