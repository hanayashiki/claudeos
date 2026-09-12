//! An interactive shell.
//!
//! Supports pipelines, redirection, `&&`/`||`/`;`/`&`, globbing, command and
//! arithmetic substitution, `if`/`while`/`until`/`for`, functions, and
//! positional parameters.

use crate::edit::{Completer, Editor};
use crate::sys;
use std::collections::{HashMap, HashSet};
use std::io::Write;

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// How a word was quoted, which decides what expansion it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quoting {
    /// Expanded, split into fields, and globbed.
    Bare,
    /// Expanded, but kept as one word.
    Double,
    /// Taken literally.
    Single,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String, Quoting),
    Pipe,
    Semi,
    Amp,
    AndIf,
    OrIf,
    Redirect(i32, RedirKind),
    Newline,
    LParen,
    RParen,
    /// `;;`, which ends a case arm.
    DoubleSemi,
    /// A here-document body, and whether its delimiter was quoted (which
    /// suppresses expansion of the body).
    HereDoc(String, bool),
}

impl Token {
    fn keyword(&self) -> Option<&str> {
        match self {
            Token::Word(text, Quoting::Bare) => Some(text.as_str()),
            _ => None,
        }
    }
    fn is_keyword(&self, want: &str) -> bool {
        self.keyword() == Some(want)
    }
}

/// How a token is named in a diagnostic: the text it was written as, not the
/// name of its variant.
impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Word(text, _) => write!(f, "`{}`", text),
            Token::Pipe => f.write_str("`|`"),
            Token::Semi => f.write_str("`;`"),
            Token::Amp => f.write_str("`&`"),
            Token::AndIf => f.write_str("`&&`"),
            Token::OrIf => f.write_str("`||`"),
            Token::Newline => f.write_str("end of line"),
            Token::LParen => f.write_str("`(`"),
            Token::RParen => f.write_str("`)`"),
            Token::DoubleSemi => f.write_str("`;;`"),
            Token::HereDoc(_, _) => f.write_str("a here-document"),
            Token::Redirect(fd, kind) => {
                let symbol = match kind {
                    RedirKind::Read => "<",
                    RedirKind::Write => ">",
                    RedirKind::Append => ">>",
                    RedirKind::Duplicate => ">&",
                };
                let implied = matches!(
                    (fd, kind),
                    (0, RedirKind::Read) | (1, RedirKind::Write) | (1, RedirKind::Append)
                );
                if implied {
                    write!(f, "`{}`", symbol)
                } else {
                    write!(f, "`{}{}`", fd, symbol)
                }
            }
        }
    }
}

const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "function",
    "return", "break", "continue", "case", "esac", "{", "}",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirKind {
    Read,
    Write,
    Append,
    /// `>&` or `<&`: the target is another descriptor.
    Duplicate,
}

/// A word made only of digits directly before a redirection names the
/// descriptor being redirected, so take it off the word being built.
fn take_pending_fd(current: &mut String, have_word: &mut bool) -> Option<i32> {
    if !*have_word || current.is_empty() || !current.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let fd = current.parse().ok();
    if fd.is_some() {
        current.clear();
        *have_word = false;
    }
    fd
}

#[derive(Debug)]
enum LexError {
    Unterminated,
}

fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut have_word = false;
    // A word is literal only when every part of it came from single quotes.
    let mut saw_single = false;
    let mut saw_other = false;
    let mut i = 0;
    // Where to resume after the newline that ends a line carrying a
    // here-document, so the body is not tokenised as commands.
    let mut resume_after_newline: Option<usize> = None;

    macro_rules! flush {
        () => {
            if have_word {
                let quoting = if saw_single && !saw_other {
                    Quoting::Single
                } else if saw_single || saw_other {
                    Quoting::Double
                } else {
                    Quoting::Bare
                };
                tokens.push(Token::Word(std::mem::take(&mut current), quoting));
                have_word = false;
                saw_single = false;
                saw_other = false;
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => {
                flush!();
                i += 1;
            }
            '\n' => {
                flush!();
                tokens.push(Token::Newline);
                i = resume_after_newline.take().unwrap_or(i + 1);
            }
            '\'' => {
                have_word = true;
                saw_single = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    current.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(LexError::Unterminated);
                }
                i += 1;
            }
            '"' => {
                have_word = true;
                saw_other = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        // The backslash survives into the word; expansion
                        // decides what it escapes. Dropping it here made
                        // "\$HOME" expand rather than print a dollar sign.
                        current.push('\\');
                        current.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    // Quoting starts over inside a command substitution, so
                    // copy the whole group through without inspecting it.
                    if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
                        let mut depth = 0;
                        let mut j = i + 1;
                        while j < chars.len() {
                            if chars[j] == '(' {
                                depth += 1;
                            } else if chars[j] == ')' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            j += 1;
                        }
                        if j >= chars.len() {
                            return Err(LexError::Unterminated);
                        }
                        current.extend(chars[i..=j].iter());
                        i = j + 1;
                        continue;
                    }
                    current.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(LexError::Unterminated);
                }
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                // Copy `$( ... )` (and `$(( ... ))`) through verbatim; it is
                // expanded after parsing.
                have_word = true;
                let mut depth = 0;
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '(' {
                        depth += 1;
                    } else if chars[j] == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(LexError::Unterminated);
                }
                current.extend(chars[i..=j].iter());
                i = j + 1;
            }
            '`' => {
                have_word = true;
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '`' {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(LexError::Unterminated);
                }
                current.extend(chars[i..=j].iter());
                i = j + 1;
            }
            '\\' if i + 1 < chars.len() => {
                if chars[i + 1] == '\n' {
                    i += 2; // line continuation
                    continue;
                }
                have_word = true;
                saw_other = true;
                current.push(chars[i + 1]);
                i += 2;
            }
            '|' => {
                flush!();
                if chars.get(i + 1) == Some(&'|') {
                    tokens.push(Token::OrIf);
                    i += 2;
                } else {
                    tokens.push(Token::Pipe);
                    i += 1;
                }
            }
            '&' => {
                flush!();
                if chars.get(i + 1) == Some(&'&') {
                    tokens.push(Token::AndIf);
                    i += 2;
                } else {
                    tokens.push(Token::Amp);
                    i += 1;
                }
            }
            ';' => {
                flush!();
                if chars.get(i + 1) == Some(&';') {
                    tokens.push(Token::DoubleSemi);
                    i += 2;
                } else {
                    tokens.push(Token::Semi);
                    i += 1;
                }
            }
            '(' => {
                flush!();
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                flush!();
                tokens.push(Token::RParen);
                i += 1;
            }
            '>' => {
                // A bare digit immediately before '>' names the descriptor.
                let fd = take_pending_fd(&mut current, &mut have_word);
                flush!();
                if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Token::Redirect(fd.unwrap_or(1), RedirKind::Append));
                    i += 2;
                } else if chars.get(i + 1) == Some(&'&') {
                    tokens.push(Token::Redirect(fd.unwrap_or(1), RedirKind::Duplicate));
                    i += 2;
                } else {
                    tokens.push(Token::Redirect(fd.unwrap_or(1), RedirKind::Write));
                    i += 1;
                }
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                flush!();
                i += 2;
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while matches!(chars.get(i), Some(' ') | Some('\t')) {
                    i += 1;
                }

                let mut delimiter = String::new();
                let mut quoted_delimiter = false;
                match chars.get(i) {
                    Some(&q @ '\'') | Some(&q @ '"') => {
                        quoted_delimiter = true;
                        i += 1;
                        while i < chars.len() && chars[i] != q {
                            delimiter.push(chars[i]);
                            i += 1;
                        }
                        i += 1;
                    }
                    _ => {
                        while i < chars.len()
                            && !chars[i].is_whitespace()
                            && !matches!(chars[i], ';' | '|' | '&')
                        {
                            delimiter.push(chars[i]);
                            i += 1;
                        }
                    }
                }

                let mut line_end = i;
                while line_end < chars.len() && chars[line_end] != '\n' {
                    line_end += 1;
                }
                if line_end >= chars.len() {
                    // The body has not been typed yet.
                    return Err(LexError::Unterminated);
                }

                let mut body = String::new();
                let mut cursor = line_end + 1;
                let mut closed = false;
                while cursor <= chars.len() {
                    let mut end = cursor;
                    while end < chars.len() && chars[end] != '\n' {
                        end += 1;
                    }
                    let raw: String = chars[cursor..end].iter().collect();
                    let line = if strip_tabs { raw.trim_start_matches('\t') } else { &raw };
                    if line == delimiter {
                        cursor = end + 1;
                        closed = true;
                        break;
                    }
                    body.push_str(line);
                    body.push('\n');
                    if end >= chars.len() {
                        cursor = end;
                        break;
                    }
                    cursor = end + 1;
                }
                if !closed {
                    return Err(LexError::Unterminated);
                }

                tokens.push(Token::HereDoc(body, quoted_delimiter));
                resume_after_newline = Some(cursor);
            }
            '<' => {
                let fd = take_pending_fd(&mut current, &mut have_word);
                flush!();
                tokens.push(Token::Redirect(fd.unwrap_or(0), RedirKind::Read));
                i += 1;
            }
            '#' if !have_word => {
                // Comment to end of line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            _ => {
                have_word = true;
                current.push(c);
                i += 1;
            }
        }
    }
    flush!();
    let _ = (have_word, saw_single, saw_other);
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------

/// Where a redirection sends a stream.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// A file, and whether to append to it.
    File(String, bool),
    /// Another descriptor, as in `2>&1`.
    Descriptor(i32),
}

#[derive(Debug, Clone, Default)]
struct Command {
    words: Vec<(String, Quoting)>,
    stdin_file: Option<String>,
    /// Redirections in the order they were written, since `>f 2>&1` and
    /// `2>&1 >f` mean different things.
    redirects: Vec<(i32, Target)>,
    heredoc: Option<(String, bool)>,
    /// A `( ... )` group, or a compound command used as a pipeline element.
    group: Option<Vec<Node>>,
    /// True when the group came from `( ... )`, which always runs in a child
    /// so its variables and directory changes do not escape.
    subshell: bool,
}

#[derive(Debug, Clone)]
struct Pipeline {
    commands: Vec<Command>,
    background: bool,
    /// `! pipeline` inverts the status.
    negated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Connector {
    Always,
    AndThen,
    OrElse,
}

#[derive(Debug, Clone)]
struct AndOr {
    items: Vec<(Connector, Pipeline)>,
}

#[derive(Debug, Clone)]
enum Node {
    Run(AndOr),
    If {
        branches: Vec<(Vec<Node>, Vec<Node>)>,
        otherwise: Vec<Node>,
    },
    Loop {
        condition: Vec<Node>,
        body: Vec<Node>,
        until: bool,
    },
    For {
        variable: String,
        words: Vec<(String, Quoting)>,
        body: Vec<Node>,
    },
    Function {
        name: String,
        body: Vec<Node>,
    },
    Case {
        word: (String, Quoting),
        arms: Vec<(Vec<(String, Quoting)>, Vec<Node>)>,
    },
    /// `{ ... }`, which runs in the current shell.
    Brace(Vec<Node>),
    Break,
    Continue,
    Return(Option<i32>),
}

#[derive(Debug)]
enum ParseError {
    /// The input ends in the middle of a construct; interactively this means
    /// "read another line".
    Incomplete,
    Message { line: usize, text: String },
}

/// The source line a token sits on. The lexer does not keep positions, so the
/// line is recovered from the stream: each newline advances one line, and a
/// here-document stands for its body plus the delimiter line that closes it.
fn line_of(tokens: &[Token], index: usize) -> usize {
    let mut line = 1;
    for token in tokens.iter().take(index) {
        match token {
            Token::Newline => line += 1,
            Token::HereDoc(body, _) => line += body.lines().count() + 1,
            _ => {}
        }
    }
    line
}

fn error_at(tokens: &[Token], index: usize, text: String) -> ParseError {
    ParseError::Message { line: line_of(tokens, index), text }
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(Token::Newline) | Some(Token::Semi)) {
            self.position += 1;
        }
    }

    fn at_any_keyword(&self, words: &[&str]) -> bool {
        if words.contains(&")") && matches!(self.peek(), Some(Token::RParen)) {
            return true;
        }
        if words.contains(&";;") && matches!(self.peek(), Some(Token::DoubleSemi)) {
            return true;
        }
        match self.peek() {
            Some(token) => token.keyword().map(|k| words.contains(&k)).unwrap_or(false),
            None => false,
        }
    }

    fn fail(&self, text: String) -> ParseError {
        error_at(self.tokens, self.position, text)
    }

    fn expect_keyword(&mut self, want: &str) -> Result<(), ParseError> {
        self.skip_separators();
        match self.peek() {
            Some(token) if token.is_keyword(want) => {
                self.position += 1;
                Ok(())
            }
            None => Err(ParseError::Incomplete),
            Some(token) => {
                let found = token.to_string();
                Err(self.fail(format!("expected `{}` but found {}", want, found)))
            }
        }
    }

    /// Parse statements until one of `terminators` is reached.
    fn parse_block(&mut self, terminators: &[&str]) -> Result<Vec<Node>, ParseError> {
        let mut nodes = Vec::new();
        loop {
            self.skip_separators();
            if self.peek().is_none() {
                if terminators.is_empty() {
                    return Ok(nodes);
                }
                return Err(ParseError::Incomplete);
            }
            if self.at_any_keyword(terminators) {
                return Ok(nodes);
            }
            let before = self.position;
            let node = self.parse_statement()?;
            if self.position == before {
                return Err(self.fail(format!("unexpected {}", self.tokens[self.position])));
            }
            nodes.push(node);
        }
    }

    fn parse_statement(&mut self) -> Result<Node, ParseError> {
        // A function definition looks like: name ( ) { ... }
        if let (Some(Token::Word(name, Quoting::Bare)), Some(Token::LParen), Some(Token::RParen)) = (
            self.tokens.get(self.position),
            self.tokens.get(self.position + 1),
            self.tokens.get(self.position + 2),
        ) {
            let name = name.clone();
            self.position += 3;
            self.skip_separators();
            self.expect_keyword("{")?;
            let body = self.parse_block(&["}"])?;
            self.expect_keyword("}")?;
            return Ok(Node::Function { name, body });
        }

        // Everything else, compound commands included, is a pipeline. Going
        // through the pipeline parser is what lets `for ...; done | wc -l`
        // and `while read l; do ...; done < file` work.
        Ok(Node::Run(self.parse_and_or()?))
    }

    /// Parse a compound command if one starts here.
    fn parse_compound(&mut self) -> Result<Option<Node>, ParseError> {
        let node = match self.peek().and_then(|t| t.keyword()) {
            Some("if") => self.parse_if()?,
            Some("while") => self.parse_loop(false)?,
            Some("until") => self.parse_loop(true)?,
            Some("for") => self.parse_for()?,
            Some("case") => self.parse_case()?,
            Some("{") => {
                self.position += 1;
                let body = self.parse_block(&["}"])?;
                self.expect_keyword("}")?;
                Node::Brace(body)
            }
            // These are commands, not statements: `cmd || break` has to reach
            // the break, and it used to parse as a separate statement that ran
            // unconditionally.
            Some("break") => {
                self.position += 1;
                Node::Break
            }
            Some("continue") => {
                self.position += 1;
                Node::Continue
            }
            Some("return") => {
                self.position += 1;
                let value = match self.peek() {
                    Some(Token::Word(text, _)) => {
                        let parsed = text.parse().ok();
                        if parsed.is_some() {
                            self.position += 1;
                        }
                        parsed
                    }
                    _ => None,
                };
                Node::Return(value)
            }
            _ => return Ok(None),
        };
        Ok(Some(node))
    }

    fn parse_if(&mut self) -> Result<Node, ParseError> {
        self.expect_keyword("if")?;
        let mut branches = Vec::new();
        let condition = self.parse_block(&["then"])?;
        self.expect_keyword("then")?;
        let body = self.parse_block(&["elif", "else", "fi"])?;
        branches.push((condition, body));

        let mut otherwise = Vec::new();
        loop {
            self.skip_separators();
            match self.peek().and_then(|t| t.keyword()) {
                Some("elif") => {
                    self.position += 1;
                    let condition = self.parse_block(&["then"])?;
                    self.expect_keyword("then")?;
                    let body = self.parse_block(&["elif", "else", "fi"])?;
                    branches.push((condition, body));
                }
                Some("else") => {
                    self.position += 1;
                    otherwise = self.parse_block(&["fi"])?;
                }
                Some("fi") => {
                    self.position += 1;
                    break;
                }
                None => return Err(ParseError::Incomplete),
                _ => {
                    return Err(self.fail(format!(
                        "unexpected {} inside `if`",
                        self.tokens[self.position]
                    )))
                }
            }
        }
        Ok(Node::If { branches, otherwise })
    }

    fn parse_loop(&mut self, until: bool) -> Result<Node, ParseError> {
        self.expect_keyword(if until { "until" } else { "while" })?;
        let condition = self.parse_block(&["do"])?;
        self.expect_keyword("do")?;
        let body = self.parse_block(&["done"])?;
        self.expect_keyword("done")?;
        Ok(Node::Loop { condition, body, until })
    }

    fn parse_for(&mut self) -> Result<Node, ParseError> {
        self.expect_keyword("for")?;
        let variable = match self.peek() {
            Some(Token::Word(name, _)) => {
                let name = name.clone();
                self.position += 1;
                name
            }
            None => return Err(ParseError::Incomplete),
            _ => return Err(self.fail("expected a name after `for`".into())),
        };

        let mut words = Vec::new();
        self.skip_separators();
        if self.at_any_keyword(&["in"]) {
            self.position += 1;
            while let Some(Token::Word(text, quoting)) = self.peek() {
                if text == "do" && *quoting == Quoting::Bare {
                    break;
                }
                words.push((text.clone(), *quoting));
                self.position += 1;
            }
        }
        self.expect_keyword("do")?;
        let body = self.parse_block(&["done"])?;
        self.expect_keyword("done")?;
        Ok(Node::For { variable, words, body })
    }

    fn parse_case(&mut self) -> Result<Node, ParseError> {
        self.expect_keyword("case")?;
        let word = match self.peek() {
            Some(Token::Word(text, quoting)) => {
                let word = (text.clone(), *quoting);
                self.position += 1;
                word
            }
            None => return Err(ParseError::Incomplete),
            _ => return Err(self.fail("expected a word after `case`".into())),
        };
        self.expect_keyword("in")?;

        let mut arms = Vec::new();
        loop {
            self.skip_separators();
            match self.peek() {
                None => return Err(ParseError::Incomplete),
                Some(token) if token.is_keyword("esac") => {
                    self.position += 1;
                    break;
                }
                _ => {}
            }

            // An arm may start with an optional '('.
            if matches!(self.peek(), Some(Token::LParen)) {
                self.position += 1;
            }
            let mut patterns = Vec::new();
            loop {
                match self.peek() {
                    Some(Token::Word(text, quoting)) => {
                        patterns.push((text.clone(), *quoting));
                        self.position += 1;
                    }
                    None => return Err(ParseError::Incomplete),
                    _ => return Err(self.fail("expected a case pattern".into())),
                }
                match self.peek() {
                    Some(Token::Pipe) => self.position += 1,
                    Some(Token::RParen) => {
                        self.position += 1;
                        break;
                    }
                    None => return Err(ParseError::Incomplete),
                    _ => {
                        return Err(self.fail("expected `)` after a case pattern".into()))
                    }
                }
            }

            let body = self.parse_block(&[";;", "esac"])?;
            if matches!(self.peek(), Some(Token::DoubleSemi)) {
                self.position += 1;
            }
            arms.push((patterns, body));
        }
        Ok(Node::Case { word, arms })
    }

    fn parse_and_or(&mut self) -> Result<AndOr, ParseError> {
        let mut items = Vec::new();
        let mut connector = Connector::Always;
        loop {
            let pipeline = self.parse_pipeline()?;
            let background = matches!(self.peek(), Some(Token::Amp));
            if background {
                self.position += 1;
            }
            items.push((connector, Pipeline { background, ..pipeline }));

            match self.peek() {
                Some(Token::AndIf) => {
                    self.position += 1;
                    connector = Connector::AndThen;
                }
                Some(Token::OrIf) => {
                    self.position += 1;
                    connector = Connector::OrElse;
                }
                _ => return Ok(AndOr { items }),
            }
            self.skip_newlines_only();
        }
    }

    fn skip_newlines_only(&mut self) {
        while matches!(self.peek(), Some(Token::Newline)) {
            self.position += 1;
        }
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut negated = false;
        while matches!(self.peek(), Some(token) if token.is_keyword("!")) {
            negated = !negated;
            self.position += 1;
        }
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek(), Some(Token::Pipe)) {
            self.position += 1;
            self.skip_newlines_only();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, background: false, negated })
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        let mut command = Command::default();

        if matches!(self.peek(), Some(Token::LParen)) {
            self.position += 1;
            command.group = Some(self.parse_block(&[")"])?);
            command.subshell = true;
            match self.peek() {
                Some(Token::RParen) => self.position += 1,
                None => return Err(ParseError::Incomplete),
                _ => return Err(self.fail("expected `)`".into())),
            }
            return self.parse_redirections(command);
        }

        // A compound command is an element of a pipeline, which is what lets
        // `seq 1 3 | while read line; do ...; done` connect, and it can carry
        // its own redirections, as in `done < file`.
        if let Some(node) = self.parse_compound()? {
            command.group = Some(vec![node]);
            return self.parse_redirections(command);
        }

        loop {
            match self.peek() {
                Some(Token::HereDoc(body, quoted)) => {
                    command.heredoc = Some((body.clone(), *quoted));
                    self.position += 1;
                }
                Some(Token::Word(text, quoting)) => {
                    // A bare keyword ends the command; the statement parser
                    // takes it from there.
                    if *quoting == Quoting::Bare && KEYWORDS.contains(&text.as_str()) {
                        return Ok(command);
                    }
                    command.words.push((text.clone(), *quoting));
                    self.position += 1;
                }
                Some(Token::Redirect(..)) => {
                    self.take_redirection(&mut command)?;
                }
                _ => return Ok(command),
            }
        }
    }

    /// Redirections that follow a group or compound command.
    fn parse_redirections(&mut self, mut command: Command) -> Result<Command, ParseError> {
        while matches!(self.peek(), Some(Token::Redirect(..))) {
            self.take_redirection(&mut command)?;
        }
        Ok(command)
    }

    fn take_redirection(&mut self, command: &mut Command) -> Result<(), ParseError> {
        let Some(Token::Redirect(fd, kind)) = self.peek().cloned() else {
            return Err(self.fail("expected a redirection".into()));
        };
        self.position += 1;

        match kind {
            RedirKind::Read => {
                command.stdin_file = Some(self.take_filename()?);
            }
            RedirKind::Write | RedirKind::Append => {
                let path = self.take_filename()?;
                command
                    .redirects
                    .push((fd, Target::File(path, kind == RedirKind::Append)));
            }
            RedirKind::Duplicate => {
                let word = self.take_filename()?;
                if word == "-" {
                    command.redirects.push((fd, Target::Descriptor(-1)));
                } else {
                    match word.parse::<i32>() {
                        Ok(target) => command.redirects.push((fd, Target::Descriptor(target))),
                        Err(_) => {
                            return Err(self.fail(format!(
                                "expected a descriptor number after `>&`, found `{}`",
                                word
                            )))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn take_filename(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(Token::Word(text, _)) => {
                let text = text.clone();
                self.position += 1;
                Ok(text)
            }
            None => Err(ParseError::Incomplete),
            _ => Err(self.fail("expected a file name after a redirection".into())),
        }
    }
}

/// Parse the whole input, returning the statements read so far alongside the
/// error that stopped it. A script runs the commands that precede a syntax
/// error, the way a shell that reads command by command does.
fn parse(tokens: &[Token]) -> (Vec<Node>, Option<ParseError>) {
    let mut parser = Parser { tokens, position: 0 };
    let mut nodes = Vec::new();
    loop {
        parser.skip_separators();
        if parser.peek().is_none() {
            return (nodes, None);
        }
        let before = parser.position;
        match parser.parse_statement() {
            Ok(node) => {
                if parser.position == before {
                    let text = format!("unexpected {}", tokens[before]);
                    return (nodes, Some(error_at(tokens, before, text)));
                }
                nodes.push(node);
            }
            // An unfinished construct means "read another line", so nothing
            // runs: the same text is parsed again once the rest arrives.
            Err(ParseError::Incomplete) => return (Vec::new(), Some(ParseError::Incomplete)),
            Err(error) => return (nodes, Some(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// What ended a block early.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
    Normal,
    Break,
    Continue,
    Return,
    Exit,
}

/// One pipeline the shell is keeping track of: either running in the
/// background or stopped in the foreground.
#[derive(Clone)]
struct Job {
    /// The number `%1` refers to. Stays with the job as others finish.
    id: usize,
    pgid: i32,
    /// Every process in the pipeline, so a stop or an exit can be waited for.
    pids: Vec<i32>,
    command: String,
    stopped: bool,
}

pub struct Shell {
    env: HashMap<String, String>,
    functions: HashMap<String, Vec<Node>>,
    positional: Vec<String>,
    last_status: i32,
    pid: i32,
    jobs: Vec<Job>,
    /// The number given to the next job. Job numbers are not reused while
    /// older jobs are still listed.
    next_job: usize,
    exit_code: Option<i32>,
    /// Set once the shell owns the terminal and puts jobs in their own groups.
    job_control: bool,
    shell_pgid: i32,
    editor: Option<Editor>,
    /// Names marked for export. A plain assignment sets a shell variable that
    /// children do not see; only these are passed on.
    exported: HashSet<String>,
    /// What `$0` expands to.
    script_name: String,
    /// What `$!` expands to.
    last_background: Option<i32>,
    /// Where `cd -` goes back to.
    previous_dir: Option<String>,
    /// `set -e`: leave on the first failure outside a condition.
    errexit: bool,
    /// How deep we are inside something whose status is being tested, where
    /// a failure is expected and must not trip `set -e`.
    condition_depth: usize,
    /// The command `trap ... EXIT` installed.
    exit_trap: Option<String>,
    /// Set when expanding a word failed, so the command is not run.
    expansion_failed: bool,
}

impl Shell {
    pub fn new() -> Shell {
        let mut env = HashMap::new();
        for (key, value) in std::env::vars() {
            env.insert(key, value);
        }
        env.entry("PATH".into()).or_insert_with(|| "/bin:/usr/bin".into());
        env.entry("HOME".into()).or_insert_with(|| "/root".into());
        // Anything inherited from the environment is exported by definition.
        let exported: HashSet<String> = env.keys().cloned().collect();
        Shell {
            env,
            functions: HashMap::new(),
            positional: Vec::new(),
            last_status: 0,
            pid: sys::getpid() as i32,
            jobs: Vec::new(),
            next_job: 1,
            exit_code: None,
            job_control: false,
            shell_pgid: 0,
            editor: None,
            exported,
            script_name: "sh".to_string(),
            last_background: None,
            previous_dir: None,
            errexit: false,
            condition_depth: 0,
            exit_trap: None,
            expansion_failed: false,
        }
    }

    fn fork_copy(&self) -> Shell {
        Shell {
            env: self.env.clone(),
            functions: self.functions.clone(),
            positional: self.positional.clone(),
            last_status: self.last_status,
            pid: sys::getpid() as i32,
            jobs: Vec::new(),
            next_job: 1,
            exit_code: None,
            job_control: false,
            shell_pgid: self.shell_pgid,
            editor: None,
            exported: self.exported.clone(),
            script_name: self.script_name.clone(),
            last_background: self.last_background,
            previous_dir: self.previous_dir.clone(),
            errexit: self.errexit,
            condition_depth: 0,
            exit_trap: None,
            expansion_failed: false,
        }
    }

    /// The environment a child should see: the exported names only.
    fn child_env(&self) -> Vec<String> {
        self.env
            .iter()
            .filter(|(name, _)| self.exported.contains(*name))
            .map(|(name, value)| format!("{}={}", name, value))
            .collect()
    }

    pub fn run(mut self, args: &[String]) -> i32 {
        if args.len() >= 3 && args[1] == "-c" {
            self.script_name = args.first().cloned().unwrap_or_else(|| "sh".into());
            self.positional = args[3..].to_vec();
            self.run_text(&args[2]);
            return self.exit_code.unwrap_or(self.last_status);
        }
        if args.len() >= 2 && !args[1].starts_with('-') {
            self.script_name = args[1].clone();
            self.positional = args[2..].to_vec();
            match std::fs::read_to_string(&args[1]) {
                Ok(text) => {
                    self.run_text(&text);
                    return self.exit_code.unwrap_or(self.last_status);
                }
                Err(err) => {
                    eprintln!("sh: {}: {}", args[1], err);
                    return 1;
                }
            }
        }
        self.interactive()
    }

    fn interactive(&mut self) -> i32 {
        // Keyboard signals belong to the foreground job, not to the shell.
        for signum in [sys::SIGINT, sys::SIGQUIT, sys::SIGTTIN, sys::SIGTTOU, sys::SIGTSTP] {
            sys::set_signal(signum, sys::SIG_IGN);
        }
        self.job_control = true;
        self.shell_pgid = sys::own_process_group();
        self.editor = Some(Editor::new());
        println!();
        println!("claudeos shell -- `help` lists the available commands");
        let mut pending = String::new();

        loop {
            self.reap_background();
            let prompt = if pending.is_empty() { self.prompt() } else { "> ".to_string() };
            let completer = self.completer();

            let line = match self.editor.as_mut() {
                Some(editor) => editor.read_line(&prompt, &completer),
                None => {
                    print!("{}", prompt);
                    let _ = std::io::stdout().flush();
                    read_line()
                }
            };
            let line = match line {
                Some(line) => line,
                None => {
                    println!();
                    return self.last_status;
                }
            };
            if pending.is_empty() {
                if let Some(editor) = self.editor.as_mut() {
                    editor.add_history(&line);
                }
            }
            pending.push_str(&line);
            pending.push('\n');

            match self.try_run(&pending) {
                Ok(()) => pending.clear(),
                Err(true) => {} // incomplete: keep reading
                Err(false) => pending.clear(),
            }
            if let Some(code) = self.exit_code {
                return code;
            }
        }
    }

    /// Snapshot of the names tab completion can offer.
    fn completer(&self) -> ShellCompleter {
        let mut commands: Vec<String> = BUILTINS.iter().map(|b| b.to_string()).collect();
        commands.extend(self.functions.keys().cloned());
        if let Some(path) = self.env.get("PATH") {
            for dir in path.split(':') {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        commands.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        commands.sort();
        commands.dedup();
        ShellCompleter { commands }
    }

    fn prompt(&self) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into());
        format!("claudeos:{}# ", cwd)
    }

    pub fn run_text(&mut self, text: &str) {
        let _ = self.try_run(text);
    }

    /// Returns Err(true) when the input is incomplete.
    fn try_run(&mut self, text: &str) -> Result<(), bool> {
        let tokens = match tokenize(text) {
            Ok(tokens) => tokens,
            Err(LexError::Unterminated) => return Err(true),
        };
        if tokens.iter().all(|t| matches!(t, Token::Newline)) {
            return Ok(());
        }
        let (nodes, error) = parse(&tokens);
        if matches!(error, Some(ParseError::Incomplete)) {
            return Err(true);
        }
        self.exec_block(&nodes);
        if let Some(ParseError::Message { line, text }) = error {
            let origin = if self.script_name == "sh" {
                String::new()
            } else {
                format!("{}: ", self.script_name)
            };
            eprintln!("sh: {}line {}: syntax error: {}", origin, line, text);
            self.last_status = 2;
            return Err(false);
        }
        Ok(())
    }

    fn exec_block(&mut self, nodes: &[Node]) -> Flow {
        for node in nodes {
            let flow = self.exec_node(node);
            if flow != Flow::Normal {
                return flow;
            }
        }
        Flow::Normal
    }

    fn exec_node(&mut self, node: &Node) -> Flow {
        match node {
            Node::Run(and_or) => self.exec_and_or(and_or),
            Node::Function { name, body } => {
                self.functions.insert(name.clone(), body.clone());
                self.last_status = 0;
                Flow::Normal
            }
            Node::If { branches, otherwise } => {
                for (condition, body) in branches {
                    self.condition_depth += 1;
                    let flow = self.exec_block(condition);
                    self.condition_depth -= 1;
                    if flow != Flow::Normal {
                        return flow;
                    }
                    if self.last_status == 0 {
                        return self.exec_block(body);
                    }
                }
                self.exec_block(otherwise)
            }
            Node::Loop { condition, body, until } => {
                let mut guard = 0u64;
                loop {
                    guard += 1;
                    if guard > 10_000_000 {
                        eprintln!("sh: loop ran too long, stopping");
                        return Flow::Normal;
                    }
                    self.condition_depth += 1;
                    let flow = self.exec_block(condition);
                    self.condition_depth -= 1;
                    if flow != Flow::Normal {
                        return flow;
                    }
                    let satisfied = if *until { self.last_status != 0 } else { self.last_status == 0 };
                    if !satisfied {
                        return Flow::Normal;
                    }
                    match self.exec_block(body) {
                        Flow::Break => return Flow::Normal,
                        Flow::Continue | Flow::Normal => {}
                        other => return other,
                    }
                }
            }
            Node::For { variable, words, body } => {
                let mut items = Vec::new();
                for (word, quoting) in words {
                    match quoting {
                        Quoting::Single => items.push(word.clone()),
                        Quoting::Double => items.push(self.expand_text(word)),
                        Quoting::Bare => items.extend(self.expand_word_bare(word)),
                    }
                }
                for item in items {
                    self.env.insert(variable.clone(), item);
                    match self.exec_block(body) {
                        Flow::Break => return Flow::Normal,
                        Flow::Continue | Flow::Normal => {}
                        other => return other,
                    }
                }
                Flow::Normal
            }
            Node::Case { word, arms } => {
                let subject = match word.1 {
                    Quoting::Single => word.0.clone(),
                    _ => self.expand_text(&word.0),
                };
                for (patterns, body) in arms {
                    let hit = patterns.iter().any(|(pattern, quoting)| {
                        let pattern = match quoting {
                            Quoting::Single => pattern.clone(),
                            _ => self.expand_text(pattern),
                        };
                        matches_pattern(&subject, &pattern)
                    });
                    if hit {
                        return self.exec_block(body);
                    }
                }
                self.last_status = 0;
                Flow::Normal
            }
            Node::Brace(body) => self.exec_block(body),
            Node::Break => Flow::Break,
            Node::Continue => Flow::Continue,
            Node::Return(code) => {
                if let Some(code) = code {
                    self.last_status = *code;
                }
                Flow::Return
            }
        }
    }

    fn exec_and_or(&mut self, and_or: &AndOr) -> Flow {
        let last = and_or.items.len().saturating_sub(1);
        for (index, (connector, pipeline)) in and_or.items.iter().enumerate() {
            match connector {
                Connector::AndThen if self.last_status != 0 => continue,
                Connector::OrElse if self.last_status == 0 => continue,
                _ => {}
            }
            // A pipeline whose status feeds && or || is being tested, so a
            // failure there is expected and must not trip `set -e`.
            let tested = index < last || pipeline.negated;
            if tested {
                self.condition_depth += 1;
            }
            let flow = self.exec_pipeline(pipeline);
            if tested {
                self.condition_depth -= 1;
            }
            if flow != Flow::Normal {
                return flow;
            }
            if self.errexit && self.last_status != 0 && self.condition_depth == 0 {
                self.exit_code = Some(self.last_status);
                return Flow::Exit;
            }
        }
        Flow::Normal
    }

    fn exec_pipeline(&mut self, pipeline: &Pipeline) -> Flow {
        self.reap_background();
        self.expansion_failed = false;
        let commands: Vec<Command> = pipeline
            .commands
            .iter()
            .map(|c| self.expand_command(c))
            .filter(|c| !c.words.is_empty() || !c.redirects.is_empty() || c.group.is_some())
            .collect();
        if self.expansion_failed {
            // A word could not be expanded, so the command does not run.
            self.expansion_failed = false;
            self.last_status = 1;
            return Flow::Normal;
        }
        if commands.is_empty() {
            return Flow::Normal;
        }

        // A single builtin or function runs here so it can change our state.
        // `A=1 B=2` on its own sets shell variables; with a command after it
        // the assignments apply only to that command.
        if commands.len() == 1 && !pipeline.background && commands[0].group.is_none() {
            let words: Vec<String> = commands[0].words.iter().map(|(w, _)| w.clone()).collect();
            if !words.is_empty() && words.iter().all(|w| is_assignment(w)) {
                for word in &words {
                    if let Some((name, value)) = word.split_once('=') {
                        self.env.insert(name.to_string(), value.to_string());
                    }
                }
                self.last_status = 0;
                return Flow::Normal;
            }
        }

        if commands.len() == 1 && !pipeline.background {
            let argv: Vec<String> = strip_assignments(&commands[0]);
            let is_local = (commands[0].group.is_some() && !commands[0].subshell)
                || (!argv.is_empty()
                    && (self.functions.contains_key(&argv[0]) || is_builtin(&argv[0])));
            if is_local {
                let flow = self.run_local(&commands[0], &argv);
                if pipeline.negated {
                    self.last_status = if self.last_status == 0 { 1 } else { 0 };
                }
                return flow;
            }
        }

        let mut pids = Vec::new();
        let mut previous_read: Option<i32> = None;
        let mut leader = 0i32;

        for (index, command) in commands.iter().enumerate() {
            let last = index + 1 == commands.len();
            let (read_end, write_end) = if last {
                (None, None)
            } else {
                match sys::pipe() {
                    Ok((r, w)) => (Some(r), Some(w)),
                    Err(err) => {
                        eprintln!("sh: pipe: {}", err);
                        self.last_status = 1;
                        return Flow::Normal;
                    }
                }
            };

            let pid = sys::fork();
            if pid == 0 {
                if self.job_control {
                    // Join the job's group and take the terminal signals back.
                    sys::setpgid(0, leader);
                    for signum in
                        [sys::SIGINT, sys::SIGQUIT, sys::SIGTTIN, sys::SIGTTOU, sys::SIGTSTP]
                    {
                        sys::set_signal(signum, sys::SIG_DFL);
                    }
                }
                if let Some(fd) = previous_read {
                    sys::dup2(fd, sys::STDIN);
                    sys::close(fd);
                }
                if let Some(fd) = write_end {
                    sys::dup2(fd, sys::STDOUT);
                    sys::close(fd);
                }
                if let Some(fd) = read_end {
                    sys::close(fd);
                }
                if apply_redirections(command).is_err() {
                    sys::exit_group(1);
                }
                if let Some(group) = &command.group {
                    // A subshell: its variables and directory changes are the
                    // child's, and vanish with it.
                    let mut child = self.fork_copy();
                    child.exec_block(group);
                    let _ = std::io::stdout().flush();
                    sys::exit_group(child.exit_code.unwrap_or(child.last_status));
                }
                let argv: Vec<String> = strip_assignments(command);
                let mut child = self.fork_copy();
                for (name, value) in leading_assignments(command) {
                    child.env.insert(name.clone(), value);
                    child.exported.insert(name);
                }
                if argv.is_empty() {
                    sys::exit_group(0);
                }
                if child.functions.contains_key(&argv[0]) || is_builtin(&argv[0]) {
                    child.run_local(command, &argv);
                    let _ = std::io::stdout().flush();
                    sys::exit_group(child.exit_code.unwrap_or(child.last_status));
                }
                let code = child.exec_external(&argv);
                sys::exit_group(code);
            }
            if pid < 0 {
                eprintln!("sh: fork failed");
                self.last_status = 1;
                return Flow::Normal;
            }

            if self.job_control {
                if leader == 0 {
                    leader = pid as i32;
                    if !pipeline.background {
                        sys::give_terminal_to(leader);
                    }
                }
                // Also set it here, so the group exists no matter which of the
                // two processes runs first.
                sys::setpgid(pid as i32, leader);
            }

            if let Some(fd) = previous_read {
                sys::close(fd);
            }
            if let Some(fd) = write_end {
                sys::close(fd);
            }
            previous_read = read_end;
            pids.push(pid as i32);
        }

        let description: Vec<String> = commands
            .iter()
            .map(|c| c.words.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>().join(" "))
            .collect();
        let description = description.join(" | ");

        if pipeline.background {
            if let Some(pid) = pids.last() {
                let job = Job {
                    id: self.next_job,
                    pgid: if leader == 0 { *pid } else { leader },
                    pids: pids.clone(),
                    command: description,
                    stopped: false,
                };
                self.next_job += 1;
                // Job control chatter belongs on stderr, so it does not end up
                // inside a command substitution.
                eprintln!("[{}] {}", job.id, pid);
                self.jobs.push(job);
                self.last_background = Some(*pid);
            }
            self.last_status = 0;
            return Flow::Normal;
        }

        let pgid = if leader == 0 { *pids.last().unwrap_or(&0) } else { leader };
        let status = self.await_foreground(pgid, &pids, &description);
        self.last_status = if pipeline.negated {
            if status == 0 {
                1
            } else {
                0
            }
        } else {
            status
        };
        Flow::Normal
    }

    /// Wait for a foreground pipeline and return its exit status. A pipeline
    /// that stopped instead of exiting is kept as a job; either way the shell
    /// takes the terminal back before returning.
    fn await_foreground(&mut self, pgid: i32, pids: &[i32], description: &str) -> i32 {
        let mut status = 0;
        let mut interrupted = false;
        let mut stopped = false;
        for pid in pids {
            let (rc, raw) = sys::wait4(*pid, sys::WUNTRACED);
            if rc < 0 {
                continue;
            }
            if let Some(signal) = sys::stop_signal_of(raw) {
                stopped = true;
                status = 128 + signal;
                continue;
            }
            status = sys::exit_code_of(raw);
            if sys::signal_of(raw) == Some(sys::SIGINT) {
                interrupted = true;
            }
        }
        if self.job_control {
            sys::give_terminal_to(self.shell_pgid);
        }
        if stopped {
            let job = Job {
                id: self.next_job,
                pgid,
                pids: pids.to_vec(),
                command: description.to_string(),
                stopped: true,
            };
            self.next_job += 1;
            eprintln!("[{}]+  Stopped  {}", job.id, job.command);
            self.jobs.push(job);
        } else if interrupted {
            println!();
        }
        status
    }

    /// Resolve `%1`, `%+`, `%-`, a bare number or a pid to a job. With no
    /// argument this is the most recent job, which is what `fg` and `bg`
    /// default to.
    fn find_job(&self, spec: Option<&String>) -> Option<usize> {
        let spec = match spec {
            None => return self.jobs.len().checked_sub(1),
            Some(spec) => spec.as_str(),
        };
        let body = spec.strip_prefix('%').unwrap_or(spec);
        if body.is_empty() || body == "+" || body == "%" {
            return self.jobs.len().checked_sub(1);
        }
        if body == "-" {
            return self.jobs.len().checked_sub(2);
        }
        if let Ok(number) = body.parse::<usize>() {
            if spec.starts_with('%') {
                return self.jobs.iter().position(|j| j.id == number);
            }
            return self
                .jobs
                .iter()
                .position(|j| j.pids.contains(&(number as i32)))
                .or_else(|| self.jobs.iter().position(|j| j.id == number));
        }
        self.jobs.iter().position(|j| j.command.starts_with(body))
    }

    /// `fg` and `bg`: send the job SIGCONT, and for `fg` wait for it again
    /// with the terminal handed over.
    fn resume_job(&mut self, spec: Option<&String>, foreground: bool) -> i32 {
        let index = match self.find_job(spec) {
            Some(index) => index,
            None => {
                eprintln!("{}: no such job", if foreground { "fg" } else { "bg" });
                return 1;
            }
        };
        let job = self.jobs.remove(index);
        if foreground {
            eprintln!("{}", job.command);
            if self.job_control {
                sys::give_terminal_to(job.pgid);
            }
            sys::kill(-job.pgid, sys::SIGCONT);
            return self.await_foreground(job.pgid, &job.pids, &job.command);
        }
        sys::kill(-job.pgid, sys::SIGCONT);
        eprintln!("[{}]+ {} &", job.id, job.command);
        self.last_background = job.pids.last().copied();
        self.jobs.push(Job { stopped: false, ..job });
        0
    }

    /// Run a builtin, a function, or a compound command in this process, with
    /// redirections applied and then undone.
    fn run_local(&mut self, command: &Command, argv: &[String]) -> Flow {
        // Save whatever the redirections are about to replace.
        let mut saved: Vec<(i32, i32)> = Vec::new();
        let mut touched: Vec<i32> = command.redirects.iter().map(|(fd, _)| *fd).collect();
        if command.stdin_file.is_some() || command.heredoc.is_some() {
            touched.push(sys::STDIN);
        }
        touched.sort();
        touched.dedup();
        for fd in &touched {
            let copy = dup_fd(*fd);
            if copy >= 0 {
                saved.push((*fd, copy));
            }
        }

        if apply_redirections(command).is_err() {
            self.last_status = 1;
            restore(&saved);
            return Flow::Normal;
        }

        let assignments = leading_assignments(command);
        let restored: Vec<(String, Option<String>)> = assignments
            .iter()
            .map(|(name, _)| (name.clone(), self.env.get(name).cloned()))
            .collect();
        for (name, value) in &assignments {
            self.env.insert(name.clone(), value.clone());
            self.exported.insert(name.clone());
        }

        let flow = if let Some(group) = &command.group {
            let group = group.clone();
            self.exec_block(&group)
        } else if let Some(body) = self.functions.get(&argv[0]).cloned() {
            let saved_params = std::mem::replace(&mut self.positional, argv[1..].to_vec());
            let flow = self.exec_block(&body);
            self.positional = saved_params;
            match flow {
                Flow::Return => Flow::Normal,
                other => other,
            }
        } else {
            self.run_builtin(argv)
        };

        for (name, previous) in restored {
            match previous {
                Some(value) => {
                    self.env.insert(name, value);
                }
                None => {
                    self.env.remove(&name);
                    self.exported.remove(&name);
                }
            }
        }

        let _ = std::io::stdout().flush();
        restore(&saved);
        flow
    }

    fn run_builtin(&mut self, argv: &[String]) -> Flow {
        self.last_status = match argv[0].as_str() {
            "cd" => {
                let here = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "/".into());
                let requested = argv.get(1).map(|s| s.as_str()).unwrap_or("");
                let target = match requested {
                    "" => self.env.get("HOME").cloned().unwrap_or("/".into()),
                    "-" => match self.previous_dir.clone() {
                        Some(previous) => {
                            println!("{}", previous);
                            previous
                        }
                        None => {
                            eprintln!("cd: OLDPWD not set");
                            return Flow::Normal;
                        }
                    },
                    other => other.to_string(),
                };
                if sys::chdir(&target) < 0 {
                    eprintln!("cd: {}: no such directory", target);
                    1
                } else {
                    self.previous_dir = Some(here.clone());
                    self.env.insert("OLDPWD".into(), here);
                    if let Ok(cwd) = std::env::current_dir() {
                        self.env.insert("PWD".into(), cwd.display().to_string());
                    }
                    0
                }
            }
            "exit" => {
                let code = argv.get(1).and_then(|a| a.parse().ok()).unwrap_or(self.last_status);
                if let Some(command) = self.exit_trap.take() {
                    self.run_text(&command);
                }
                self.exit_code = Some(code);
                return Flow::Exit;
            }
            "pwd" => match std::env::current_dir() {
                Ok(dir) => {
                    println!("{}", dir.display());
                    0
                }
                Err(err) => {
                    eprintln!("pwd: {}", err);
                    1
                }
            },
            "export" => {
                if argv.len() == 1 {
                    let mut names: Vec<&String> = self.exported.iter().collect();
                    names.sort();
                    for name in names {
                        println!("export {}={}", name, self.lookup(name));
                    }
                    return Flow::Normal;
                }
                for assignment in &argv[1..] {
                    match assignment.split_once('=') {
                        Some((key, value)) => {
                            self.env.insert(key.to_string(), value.to_string());
                            self.exported.insert(key.to_string());
                        }
                        None => {
                            self.exported.insert(assignment.clone());
                        }
                    }
                }
                0
            }
            "unset" => {
                for key in &argv[1..] {
                    self.env.remove(key);
                    self.exported.remove(key);
                }
                0
            }
            // A local is a plain variable here; there is no function scope.
            "local" => {
                for assignment in &argv[1..] {
                    if let Some((key, value)) = assignment.split_once('=') {
                        self.env.insert(key.to_string(), value.to_string());
                    }
                }
                0
            }
            "set" => {
                if argv.len() == 1 {
                    let mut keys: Vec<&String> = self.env.keys().collect();
                    keys.sort();
                    for key in keys {
                        println!("{}={}", key, self.env[key]);
                    }
                    return Flow::Normal;
                }
                let mut index = 1;
                while index < argv.len() {
                    match argv[index].as_str() {
                        "--" => {
                            self.positional = argv[index + 1..].to_vec();
                            index = argv.len();
                        }
                        "-e" => {
                            self.errexit = true;
                            index += 1;
                        }
                        "+e" => {
                            self.errexit = false;
                            index += 1;
                        }
                        flag if flag.starts_with('-') || flag.starts_with('+') => {
                            // Other options are accepted and ignored.
                            index += 1;
                        }
                        _ => {
                            self.positional = argv[index..].to_vec();
                            index = argv.len();
                        }
                    }
                }
                0
            }
            "shift" => {
                let count = argv.get(1).and_then(|a| a.parse().ok()).unwrap_or(1usize);
                if count <= self.positional.len() {
                    self.positional.drain(..count);
                    0
                } else {
                    1
                }
            }
            "trap" => {
                // Only the EXIT trap is honoured; the rest are accepted.
                if argv.len() >= 3 && argv[2..].iter().any(|s| s == "EXIT" || s == "0") {
                    self.exit_trap = Some(argv[1].clone());
                }
                0
            }
            "wait" => {
                loop {
                    let (pid, _) = sys::wait4(-1, 0);
                    if pid <= 0 {
                        break;
                    }
                }
                0
            }
            "command" => {
                if argv.len() < 2 {
                    return Flow::Normal;
                }
                if argv[1] == "-v" || argv[1] == "-V" {
                    match argv.get(2) {
                        Some(name) if is_builtin(name) => println!("{}", name),
                        Some(name) => match self.find_in_path(name) {
                            Some(path) => println!("{}", path),
                            None => {
                                self.last_status = 1;
                                return Flow::Normal;
                            }
                        },
                        None => {}
                    }
                    return Flow::Normal;
                }
                // Run it without consulting the function table.
                let rest: Vec<String> = argv[1..].to_vec();
                if is_builtin(&rest[0]) {
                    return self.run_builtin(&rest);
                }
                let pid = sys::fork();
                if pid == 0 {
                    let code = self.exec_external(&rest);
                    sys::exit_group(code);
                }
                let (_, status) = sys::wait4(pid as i32, 0);
                sys::exit_code_of(status)
            }
            "exec" => {
                if argv.len() < 2 {
                    return Flow::Normal;
                }
                let rest: Vec<String> = argv[1..].to_vec();
                let code = self.exec_external(&rest);
                // exec_external only returns when the program could not start.
                self.exit_code = Some(code);
                return Flow::Exit;
            }
            "fg" => self.resume_job(argv.get(1), true),
            "bg" => self.resume_job(argv.get(1), false),
            "kill" => {
                let mut signal = 15;
                let mut targets = Vec::new();
                for argument in &argv[1..] {
                    if let Some(rest) = argument.strip_prefix('-') {
                        if let Ok(number) = rest.parse::<i32>() {
                            signal = number;
                            continue;
                        }
                        signal = match rest.trim_start_matches("SIG") {
                            "HUP" => 1,
                            "INT" => 2,
                            "QUIT" => 3,
                            "KILL" => 9,
                            "TERM" => 15,
                            "CONT" => 18,
                            "STOP" => 19,
                            "TSTP" => 20,
                            _ => signal,
                        };
                        continue;
                    }
                    if argument.starts_with('%') {
                        match self.find_job(Some(argument)) {
                            Some(index) => targets.push(-self.jobs[index].pgid),
                            None => eprintln!("kill: {}: no such job", argument),
                        }
                        continue;
                    }
                    match argument.parse::<i32>() {
                        Ok(pid) => targets.push(pid),
                        Err(_) => eprintln!("kill: {}: not a process id", argument),
                    }
                }
                if targets.is_empty() {
                    eprintln!("usage: kill [-SIGNAL] pid...");
                    2
                } else {
                    let mut status = 0;
                    for pid in targets {
                        if sys::kill(pid, signal) < 0 {
                            eprintln!("kill: {}: no such process", pid);
                            status = 1;
                        }
                    }
                    status
                }
            }
            "read" => {
                let name = argv.get(1).cloned().unwrap_or_else(|| "REPLY".into());
                match read_line() {
                    Some(line) => {
                        self.env.insert(name, line);
                        0
                    }
                    None => 1,
                }
            }
            "jobs" => {
                let last = self.jobs.len();
                for (index, job) in self.jobs.iter().enumerate() {
                    let mark = if index + 1 == last {
                        '+'
                    } else if index + 2 == last {
                        '-'
                    } else {
                        ' '
                    };
                    let state = if job.stopped { "Stopped" } else { "Running" };
                    println!("[{}]{}  {}  {}", job.id, mark, state, job.command);
                }
                0
            }
            "type" | "which" => {
                for name in &argv[1..] {
                    if self.functions.contains_key(name) {
                        println!("{} is a shell function", name);
                    } else if is_builtin(name) {
                        println!("{} is a shell builtin", name);
                    } else if let Some(path) = self.find_in_path(name) {
                        println!("{}", path);
                    } else {
                        println!("{}: not found", name);
                    }
                }
                0
            }
            "." | "source" => match argv.get(1) {
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(text) => {
                        self.run_text(&text);
                        self.last_status
                    }
                    Err(err) => {
                        eprintln!("source: {}: {}", path, err);
                        1
                    }
                },
                None => 1,
            },
            "help" => {
                crate::help();
                0
            }
            "history" => {
                if let Some(editor) = self.editor.as_ref() {
                    for (index, line) in editor.history().iter().enumerate() {
                        println!("{:>5}  {}", index + 1, line);
                    }
                }
                0
            }
            ":" => 0,
            _ => {
                if let Some((key, value)) = argv[0].split_once('=') {
                    self.env.insert(key.to_string(), value.to_string());
                    0
                } else {
                    127
                }
            }
        };
        Flow::Normal
    }

    /// Search PATH and exec. Only returns if the program could not be started.
    fn exec_external(&self, argv: &[String]) -> i32 {
        let _ = &self.exported;
        const ENOENT: i64 = -2;
        const ENOEXEC: i64 = -8;
        const EACCES: i64 = -13;
        const EISDIR: i64 = -21;

        let envp = self.child_env();
        let candidates: Vec<String> = if argv[0].contains('/') {
            vec![argv[0].clone()]
        } else {
            let path = self.env.get("PATH").cloned().unwrap_or_else(|| "/bin".into());
            path.split(':').map(|dir| format!("{}/{}", dir, argv[0])).collect()
        };

        // Remember the most specific failure: "not found" is the least
        // informative answer and should not hide a real one.
        let mut reason = ENOENT;
        for candidate in &candidates {
            let error = sys::execve(candidate, argv, &envp);
            if error == ENOEXEC {
                // Not an executable image. A shell runs such a file itself,
                // which is what makes a script without a `#!` line work.
                let mut script = vec!["/bin/sh".to_string(), candidate.clone()];
                script.extend(argv[1..].iter().cloned());
                sys::execve("/bin/sh", &script, &envp);
            }
            if error != ENOENT {
                reason = error;
            }
        }

        match reason {
            EACCES => {
                // execve reports EACCES for a directory too, so say which.
                let is_dir = candidates
                    .iter()
                    .any(|c| std::fs::metadata(c).map(|m| m.is_dir()).unwrap_or(false));
                if is_dir {
                    eprintln!("sh: {}: is a directory", argv[0]);
                } else {
                    eprintln!("sh: {}: permission denied", argv[0]);
                }
                126
            }
            EISDIR => {
                eprintln!("sh: {}: is a directory", argv[0]);
                126
            }
            ENOEXEC => {
                eprintln!("sh: {}: cannot execute", argv[0]);
                126
            }
            _ => {
                eprintln!("sh: {}: not found", argv[0]);
                127
            }
        }
    }

    fn find_in_path(&self, name: &str) -> Option<String> {
        if name.contains('/') {
            return std::fs::metadata(name).ok().map(|_| name.to_string());
        }
        for dir in self.env.get("PATH")?.split(':') {
            let candidate = format!("{}/{}", dir, name);
            if std::fs::metadata(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        None
    }

    fn reap_background(&mut self) {
        loop {
            let (pid, status) = sys::wait4(-1, sys::WNOHANG | sys::WUNTRACED | sys::WCONTINUED);
            if pid <= 0 {
                break;
            }
            let index = match self.jobs.iter().position(|j| j.pids.contains(&(pid as i32))) {
                Some(index) => index,
                None => continue,
            };
            if sys::stop_signal_of(status).is_some() {
                self.jobs[index].stopped = true;
                eprintln!("[{}]+  Stopped  {}", self.jobs[index].id, self.jobs[index].command);
                continue;
            }
            if sys::is_continued(status) {
                self.jobs[index].stopped = false;
                continue;
            }
            self.jobs[index].pids.retain(|p| *p != pid as i32);
            if self.jobs[index].pids.is_empty() {
                let job = self.jobs.remove(index);
                eprintln!("[done] {} ({})", job.command, sys::exit_code_of(status));
            }
        }
    }

    // -- expansion ---------------------------------------------------------

    fn expand_command(&mut self, command: &Command) -> Command {
        let mut out = Command {
            stdin_file: command.stdin_file.as_ref().map(|f| self.expand_text(f)),
            redirects: command
                .redirects
                .iter()
                .map(|(fd, target)| {
                    let target = match target {
                        Target::File(path, append) => {
                            Target::File(self.expand_text(path), *append)
                        }
                        other => other.clone(),
                    };
                    (*fd, target)
                })
                .collect(),
            // An unquoted here-document delimiter means the body is expanded.
            heredoc: command.heredoc.as_ref().map(|(body, quoted)| {
                let text = if *quoted { body.clone() } else { self.expand_text(body) };
                (text, *quoted)
            }),
            group: command.group.clone(),
            subshell: command.subshell,
            words: Vec::new(),
        };
        for (word, quoting) in &command.words {
            match quoting {
                Quoting::Single => out.words.push((word.clone(), Quoting::Single)),
                Quoting::Double => {
                    // "$@" becomes one word per parameter.
                    if word == "$@" {
                        for parameter in &self.positional {
                            out.words.push((parameter.clone(), Quoting::Double));
                        }
                        continue;
                    }
                    out.words.push((self.expand_text(word), Quoting::Double));
                }
                Quoting::Bare => {
                    for expanded in self.expand_word_bare(word) {
                        out.words.push((expanded, Quoting::Bare));
                    }
                }
            }
        }
        out
    }

    /// Expand a word in one pass.
    ///
    /// Everything happens in a single left-to-right walk, so text produced by
    /// a command substitution is never rescanned: `$(echo '$HOME')` yields the
    /// three characters, not the value of HOME.
    fn expand_text(&mut self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut i = 0;

        while i < chars.len() {
            let c = chars[i];

            if c == '~' && i == 0 && (chars.len() == 1 || chars[1] == '/') {
                out.push_str(self.env.get("HOME").map(|s| s.as_str()).unwrap_or("/root"));
                i += 1;
                continue;
            }
            if c == '\\' && i + 1 < chars.len() {
                // Inside double quotes a backslash is only special before
                // these; anywhere else both characters stand.
                let next = chars[i + 1];
                if matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    if next != '\n' {
                        out.push(next);
                    }
                } else {
                    out.push('\\');
                    out.push(next);
                }
                i += 2;
                continue;
            }
            if c == '`' {
                if let Some(offset) = chars[i + 1..].iter().position(|&x| x == '`') {
                    let end = i + 1 + offset;
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.push_str(&self.capture(&inner));
                    i = end + 1;
                    continue;
                }
            }
            if c == '$' && chars.get(i + 1) == Some(&'(') {
                // $(( ... )) is arithmetic; $( ... ) runs a command.
                if chars.get(i + 2) == Some(&'(') {
                    if let Some(end) = find_close(&chars, i + 3, '(', ')') {
                        if chars.get(end + 1) == Some(&')') {
                            let inner: String = chars[i + 3..end].iter().collect();
                            let expanded = self.expand_text(&inner);
                            let resolved = self.resolve_names(&expanded);
                            match arithmetic(&resolved) {
                                Ok(value) => out.push_str(&value.to_string()),
                                Err(message) => {
                                    eprintln!("sh: {}", message);
                                    self.expansion_failed = true;
                                }
                            }
                            i = end + 2;
                            continue;
                        }
                    }
                }
                if let Some(end) = find_close(&chars, i + 2, '(', ')') {
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push_str(&self.capture(&inner));
                    i = end + 1;
                    continue;
                }
            }
            if c == '$' && i + 1 < chars.len() {
                i += 1;
                match chars[i] {
                    '?' => {
                        out.push_str(&self.last_status.to_string());
                        i += 1;
                    }
                    '$' => {
                        out.push_str(&self.pid.to_string());
                        i += 1;
                    }
                    '#' => {
                        out.push_str(&self.positional.len().to_string());
                        i += 1;
                    }
                    '@' | '*' => {
                        out.push_str(&self.positional.join(" "));
                        i += 1;
                    }
                    '0' => {
                        out.push_str(&self.script_name);
                        i += 1;
                    }
                    '!' => {
                        if let Some(pid) = self.last_background {
                            out.push_str(&pid.to_string());
                        }
                        i += 1;
                    }
                    digit if digit.is_ascii_digit() => {
                        let index = digit.to_digit(10).unwrap() as usize;
                        if let Some(value) = self.positional.get(index - 1) {
                            out.push_str(value);
                        }
                        i += 1;
                    }
                    '{' => {
                        i += 1;
                        let mut body = String::new();
                        let mut depth = 1;
                        while i < chars.len() {
                            if chars[i] == '{' {
                                depth += 1;
                            } else if chars[i] == '}' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            body.push(chars[i]);
                            i += 1;
                        }
                        i += 1;
                        out.push_str(&self.expand_braced(&body));
                    }
                    letter if letter.is_alphanumeric() || letter == '_' => {
                        let mut name = String::new();
                        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                            name.push(chars[i]);
                            i += 1;
                        }
                        let value = self.lookup(&name).to_string();
                        out.push_str(&value);
                    }
                    _ => out.push('$'),
                }
                continue;
            }

            out.push(c);
            i += 1;
        }
        out
    }

    /// Expand an unquoted word: substitution, then field splitting, then
    /// globbing of each field.
    fn expand_word_bare(&mut self, word: &str) -> Vec<String> {
        let expanded = self.expand_text(word);
        let fields: Vec<String> = if expanded.contains(char::is_whitespace) {
            expanded.split_whitespace().map(|f| f.to_string()).collect()
        } else {
            vec![expanded]
        };
        let mut out = Vec::new();
        for field in fields {
            match glob(&field) {
                Some(matches) if !matches.is_empty() => out.extend(matches),
                _ => out.push(field),
            }
        }
        out
    }

    fn lookup(&self, name: &str) -> &str {
        self.env.get(name).map(|s| s.as_str()).unwrap_or("")
    }

    /// The inside of `${ ... }`: a name, a length, or a name with a modifier.
    fn expand_braced(&mut self, body: &str) -> String {
        if let Some(name) = body.strip_prefix('#') {
            return self.lookup(name).chars().count().to_string();
        }

        // ${name:-word}, :=, :+, :?  and the same four without the colon.
        let operators = [":-", ":=", ":+", ":?", "-", "=", "+", "?"];
        for operator in operators {
            if let Some(index) = body.find(operator) {
                // A name never contains an operator character, so the first
                // hit is the real split point.
                let name = &body[..index];
                if name.is_empty() || !is_name(name) {
                    continue;
                }
                let word = &body[index + operator.len()..];
                let current = self.env.get(name).cloned();
                let unset_or_empty = match &current {
                    None => true,
                    Some(value) => operator.starts_with(':') && value.is_empty(),
                };

                return match operator.trim_start_matches(':') {
                    "-" => {
                        if unset_or_empty {
                            self.expand_text(word)
                        } else {
                            current.unwrap_or_default()
                        }
                    }
                    "=" => {
                        if unset_or_empty {
                            let value = self.expand_text(word);
                            self.env.insert(name.to_string(), value.clone());
                            value
                        } else {
                            current.unwrap_or_default()
                        }
                    }
                    "+" => {
                        if unset_or_empty {
                            String::new()
                        } else {
                            self.expand_text(word)
                        }
                    }
                    _ => {
                        if unset_or_empty {
                            let message = self.expand_text(word);
                            eprintln!(
                                "sh: {}: {}",
                                name,
                                if message.is_empty() { "parameter not set" } else { &message }
                            );
                            self.expansion_failed = true;
                            String::new()
                        } else {
                            current.unwrap_or_default()
                        }
                    }
                };
            }
        }
        self.lookup(body).to_string()
    }

    /// Inside `$(( ))` a bare name is a variable reference, so replace each
    /// identifier with its value (or zero when it is unset).
    fn resolve_names(&self, expression: &str) -> String {
        let chars: Vec<char> = expression.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let value = self.lookup(&name);
                out.push_str(if value.is_empty() { "0" } else { value });
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// Run `text` in a child and return what it wrote to standard output.
    fn capture(&mut self, text: &str) -> String {
        let (read_fd, write_fd) = match sys::pipe() {
            Ok(pair) => pair,
            Err(_) => return String::new(),
        };
        let pid = sys::fork();
        if pid == 0 {
            sys::close(read_fd);
            sys::dup2(write_fd, sys::STDOUT);
            sys::close(write_fd);
            let mut child = self.fork_copy();
            child.run_text(text);
            let _ = std::io::stdout().flush();
            sys::exit_group(child.exit_code.unwrap_or(child.last_status));
        }
        sys::close(write_fd);
        if pid < 0 {
            sys::close(read_fd);
            return String::new();
        }

        let mut collected = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = sys::read(read_fd, &mut buffer);
            if n <= 0 {
                break;
            }
            collected.extend_from_slice(&buffer[..n as usize]);
        }
        sys::close(read_fd);
        let (_, status) = sys::wait4(pid as i32, 0);
        self.last_status = sys::exit_code_of(status);

        let mut captured = String::from_utf8_lossy(&collected).to_string();
        while captured.ends_with('\n') || captured.ends_with('\r') {
            captured.pop();
        }
        captured
    }
}

const BUILTINS: &[&str] = &[
    "cd", "exit", "pwd", "export", "unset", "set", "read", "jobs", "kill", "type", "which", ".",
    "source", "help", "history", ":", "local", "wait", "command", "exec", "trap", "shift",
    "fg", "bg",
];

fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name) || is_assignment(name)
}

/// A word of the form NAME=value.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => is_name(name),
        None => false,
    }
}

fn leading_assignments(command: &Command) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (word, _) in &command.words {
        match word.split_once('=') {
            Some((name, value)) if is_name(name) => {
                out.push((name.to_string(), value.to_string()))
            }
            _ => break,
        }
    }
    out
}

fn strip_assignments(command: &Command) -> Vec<String> {
    command
        .words
        .iter()
        .map(|(word, _)| word.clone())
        .skip_while(|word| is_assignment(word))
        .collect()
}

/// Completes command names for the first word and file names elsewhere.
struct ShellCompleter {
    commands: Vec<String>,
}

impl Completer for ShellCompleter {
    fn candidates(&self, word: &str, is_command: bool) -> Vec<String> {
        if is_command && !word.contains('/') {
            return self
                .commands
                .iter()
                .filter(|name| name.starts_with(word))
                .cloned()
                .collect();
        }

        let (dir, prefix) = match word.rfind('/') {
            Some(index) => (&word[..index + 1], &word[index + 1..]),
            None => ("", word),
        };
        let search = if dir.is_empty() { "." } else { dir };
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(search) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(prefix) || (prefix.is_empty() && name.starts_with('.')) {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                out.push(format!("{}{}{}", dir, name, if is_dir { "/" } else { "" }));
            }
        }
        out.sort();
        out
    }
}

fn dup_fd(fd: i32) -> i32 {
    // dup2 onto a high descriptor stands in for dup(); the shell never opens
    // anywhere near that many files.
    let target = 200 + fd;
    if sys::dup2(fd, target) < 0 {
        -1
    } else {
        target
    }
}

fn restore(saved: &[(i32, i32)]) {
    for (fd, copy) in saved {
        sys::dup2(*copy, *fd);
        sys::close(*copy);
    }
}

fn apply_redirections(command: &Command) -> Result<(), ()> {
    if let Some((body, _)) = &command.heredoc {
        // Staged through a file rather than a pipe, so a body larger than the
        // pipe buffer cannot deadlock against a reader that has not started.
        let path = format!("/tmp/.heredoc-{}", sys::getpid());
        if std::fs::write(&path, body).is_err() {
            eprintln!("sh: cannot stage here-document");
            return Err(());
        }
        let fd = sys::open(&path, sys::O_RDONLY, 0);
        let _ = std::fs::remove_file(&path);
        if fd < 0 {
            eprintln!("sh: cannot open here-document");
            return Err(());
        }
        sys::dup2(fd as i32, sys::STDIN);
        sys::close(fd as i32);
    }
    if let Some(path) = &command.stdin_file {
        let fd = sys::open(path, sys::O_RDONLY, 0);
        if fd < 0 {
            eprintln!("sh: {}: cannot open", path);
            return Err(());
        }
        sys::dup2(fd as i32, sys::STDIN);
        sys::close(fd as i32);
    }

    // In order: `>f 2>&1` and `2>&1 >f` do different things.
    for (fd, target) in &command.redirects {
        match target {
            Target::File(path, append) => {
                let flags = sys::O_WRONLY
                    | sys::O_CREAT
                    | if *append { sys::O_APPEND } else { sys::O_TRUNC };
                let opened = sys::open(path, flags, 0o644);
                if opened < 0 {
                    eprintln!("sh: {}: cannot create", path);
                    return Err(());
                }
                sys::dup2(opened as i32, *fd);
                sys::close(opened as i32);
            }
            Target::Descriptor(-1) => {
                sys::close(*fd);
            }
            Target::Descriptor(source) => {
                if sys::dup2(*source, *fd) < 0 {
                    eprintln!("sh: {}: bad descriptor", source);
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

fn find_close(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == open {
            depth += 1;
        } else if chars[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Evaluate an integer expression for `$(( ... ))`.
fn arithmetic(expression: &str) -> Result<i64, String> {
    let tokens: Vec<char> = expression.chars().filter(|c| !c.is_whitespace()).collect();
    let mut position = 0;
    parse_sum(&tokens, &mut position)
}

fn parse_sum(tokens: &[char], position: &mut usize) -> Result<i64, String> {
    let mut value = parse_product(tokens, position)?;
    while *position < tokens.len() {
        match tokens[*position] {
            '+' => {
                *position += 1;
                value = value.wrapping_add(parse_product(tokens, position)?);
            }
            '-' => {
                *position += 1;
                value = value.wrapping_sub(parse_product(tokens, position)?);
            }
            _ => break,
        }
    }
    Ok(value)
}

fn parse_product(tokens: &[char], position: &mut usize) -> Result<i64, String> {
    let mut value = parse_atom(tokens, position)?;
    while *position < tokens.len() {
        let operator = tokens[*position];
        if !matches!(operator, '*' | '/' | '%') {
            break;
        }
        *position += 1;
        let operand = parse_atom(tokens, position)?;
        if matches!(operator, '/' | '%') && operand == 0 {
            return Err("division by 0".to_string());
        }
        value = match operator {
            '*' => value.wrapping_mul(operand),
            '/' => value / operand,
            _ => value % operand,
        };
    }
    Ok(value)
}

fn parse_atom(tokens: &[char], position: &mut usize) -> Result<i64, String> {
    if *position >= tokens.len() {
        return Ok(0);
    }
    match tokens[*position] {
        '(' => {
            *position += 1;
            let value = parse_sum(tokens, position)?;
            if tokens.get(*position) == Some(&')') {
                *position += 1;
            }
            Ok(value)
        }
        '-' => {
            *position += 1;
            Ok(-parse_atom(tokens, position)?)
        }
        _ => {
            let start = *position;
            while *position < tokens.len() && tokens[*position].is_ascii_digit() {
                *position += 1;
            }
            if start == *position {
                return Err(format!("unexpected `{}` in an expression", tokens[start]));
            }
            Ok(tokens[start..*position].iter().collect::<String>().parse().unwrap_or(0))
        }
    }
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !text.starts_with(|c: char| c.is_ascii_digit())
}

/// Expand a `*`/`?` pattern against the filesystem.
fn glob(pattern: &str) -> Option<Vec<String>> {
    let (dir, file_pattern) = match pattern.rfind('/') {
        Some(index) => (&pattern[..index + 1], &pattern[index + 1..]),
        None => ("", pattern),
    };
    if !file_pattern.contains('*') && !file_pattern.contains('?') && !file_pattern.contains('[')
    {
        return None;
    }
    let search_dir = if dir.is_empty() { "." } else { dir };
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(search_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !file_pattern.starts_with('.') {
            continue;
        }
        if matches_pattern(&name, file_pattern) {
            matches.push(format!("{}{}", dir, name));
        }
    }
    matches.sort();
    Some(matches)
}

/// Match one character against a `[...]` class, returning the index just past
/// the closing bracket.
pub fn match_class(pattern: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = matches!(pattern.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < pattern.len() {
        if pattern[i] == ']' && !first {
            return Some((hit != negated, i + 1));
        }
        first = false;
        let low = pattern[i];
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|&x| x != ']') {
            let high = pattern[i + 2];
            if c >= low && c <= high {
                hit = true;
            }
            i += 3;
        } else {
            if c == low {
                hit = true;
            }
            i += 1;
        }
    }
    // No closing bracket: the '[' was a literal.
    None
}

pub fn matches_pattern(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ni, mut pi) = (0usize, 0usize);
    let (mut star, mut backtrack) = (usize::MAX, 0usize);

    while ni < n.len() {
        if pi < p.len() && p[pi] == '[' {
            if let Some((hit, next)) = match_class(&p, pi, n[ni]) {
                if hit {
                    ni += 1;
                    pi = next;
                    continue;
                }
                if star != usize::MAX {
                    pi = star + 1;
                    backtrack += 1;
                    ni = backtrack;
                    continue;
                }
                return false;
            }
        }
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            ni += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            backtrack = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            backtrack += 1;
            ni = backtrack;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Read one line from standard input. Returns None at end of file.
fn read_line() -> Option<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = sys::read(sys::STDIN, &mut byte);
        if n <= 0 {
            if out.is_empty() {
                return None;
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&out).to_string())
}
