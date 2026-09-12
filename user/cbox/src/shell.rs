//! An interactive shell.
//!
//! Supports pipelines, redirection, `&&`/`||`/`;`/`&`, globbing, command and
//! arithmetic substitution, `if`/`while`/`until`/`for`, functions, and
//! positional parameters.

use crate::edit::{Completer, Editor};
use crate::sys;
use std::collections::HashMap;
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
    RedirOut,
    RedirAppend,
    RedirIn,
    RedirErr,
    RedirErrAppend,
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

const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "function",
    "return", "break", "continue", "case", "esac", "!", "{", "}",
];

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
                        i += 1;
                        current.push(chars[i]);
                        i += 1;
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
                flush!();
                if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Token::RedirAppend);
                    i += 2;
                } else {
                    tokens.push(Token::RedirOut);
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
                flush!();
                tokens.push(Token::RedirIn);
                i += 1;
            }
            '2' if !have_word && chars.get(i + 1) == Some(&'>') => {
                if chars.get(i + 2) == Some(&'>') {
                    tokens.push(Token::RedirErrAppend);
                    i += 3;
                } else {
                    tokens.push(Token::RedirErr);
                    i += 2;
                }
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

#[derive(Debug, Clone, Default)]
struct Command {
    words: Vec<(String, Quoting)>,
    stdin_file: Option<String>,
    stdout_file: Option<(String, bool)>,
    stderr_file: Option<(String, bool)>,
    heredoc: Option<(String, bool)>,
    /// A `( ... )` group, which runs in a child so its effects are discarded.
    group: Option<Vec<Node>>,
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
    Break,
    Continue,
    Return(Option<i32>),
}

#[derive(Debug)]
enum ParseError {
    /// The input ends in the middle of a construct; interactively this means
    /// "read another line".
    Incomplete,
    Message(String),
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

    fn expect_keyword(&mut self, want: &str) -> Result<(), ParseError> {
        self.skip_separators();
        match self.peek() {
            Some(token) if token.is_keyword(want) => {
                self.position += 1;
                Ok(())
            }
            None => Err(ParseError::Incomplete),
            Some(token) => Err(ParseError::Message(format!(
                "expected `{}` but found {:?}",
                want, token
            ))),
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
                return Err(ParseError::Message(format!(
                    "unexpected {:?}",
                    self.tokens[self.position]
                )));
            }
            nodes.push(node);
        }
    }

    fn parse_statement(&mut self) -> Result<Node, ParseError> {
        match self.peek().and_then(|t| t.keyword()) {
            Some("if") => self.parse_if(),
            Some("while") => self.parse_loop(false),
            Some("until") => self.parse_loop(true),
            Some("for") => self.parse_for(),
            Some("case") => self.parse_case(),
            Some("break") => {
                self.position += 1;
                Ok(Node::Break)
            }
            Some("continue") => {
                self.position += 1;
                Ok(Node::Continue)
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
                Ok(Node::Return(value))
            }
            _ => {
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
                Ok(Node::Run(self.parse_and_or()?))
            }
        }
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
                other => {
                    return Err(ParseError::Message(format!(
                        "unexpected {:?} inside if",
                        other
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
            _ => return Err(ParseError::Message("expected a name after `for`".into())),
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
            _ => return Err(ParseError::Message("expected a word after `case`".into())),
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
                    _ => return Err(ParseError::Message("expected a case pattern".into())),
                }
                match self.peek() {
                    Some(Token::Pipe) => self.position += 1,
                    Some(Token::RParen) => {
                        self.position += 1;
                        break;
                    }
                    None => return Err(ParseError::Incomplete),
                    _ => {
                        return Err(ParseError::Message(
                            "expected `)` after a case pattern".into(),
                        ))
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
            match self.peek() {
                Some(Token::RParen) => self.position += 1,
                None => return Err(ParseError::Incomplete),
                _ => return Err(ParseError::Message("expected `)`".into())),
            }
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
                    if command.group.is_some() {
                        return Ok(command);
                    }
                    command.words.push((text.clone(), *quoting));
                    self.position += 1;
                }
                Some(Token::RedirOut) | Some(Token::RedirAppend) => {
                    let append = matches!(self.peek(), Some(Token::RedirAppend));
                    self.position += 1;
                    command.stdout_file = Some((self.take_filename()?, append));
                }
                Some(Token::RedirErr) | Some(Token::RedirErrAppend) => {
                    let append = matches!(self.peek(), Some(Token::RedirErrAppend));
                    self.position += 1;
                    command.stderr_file = Some((self.take_filename()?, append));
                }
                Some(Token::RedirIn) => {
                    self.position += 1;
                    command.stdin_file = Some(self.take_filename()?);
                }
                _ => return Ok(command),
            }
        }
    }

    fn take_filename(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(Token::Word(text, _)) => {
                let text = text.clone();
                self.position += 1;
                Ok(text)
            }
            None => Err(ParseError::Incomplete),
            _ => Err(ParseError::Message("expected a file name after a redirection".into())),
        }
    }
}

fn parse(tokens: &[Token]) -> Result<Vec<Node>, ParseError> {
    let mut parser = Parser { tokens, position: 0 };
    let nodes = parser.parse_block(&[])?;
    if parser.position < tokens.len() {
        return Err(ParseError::Message(format!(
            "unexpected {:?}",
            tokens[parser.position]
        )));
    }
    Ok(nodes)
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

pub struct Shell {
    env: HashMap<String, String>,
    functions: HashMap<String, Vec<Node>>,
    positional: Vec<String>,
    last_status: i32,
    pid: i32,
    jobs: Vec<(i32, String)>,
    exit_code: Option<i32>,
    /// Set once the shell owns the terminal and puts jobs in their own groups.
    job_control: bool,
    shell_pgid: i32,
    editor: Option<Editor>,
}

impl Shell {
    pub fn new() -> Shell {
        let mut env = HashMap::new();
        for (key, value) in std::env::vars() {
            env.insert(key, value);
        }
        env.entry("PATH".into()).or_insert_with(|| "/bin:/usr/bin".into());
        env.entry("HOME".into()).or_insert_with(|| "/root".into());
        Shell {
            env,
            functions: HashMap::new(),
            positional: Vec::new(),
            last_status: 0,
            pid: sys::getpid() as i32,
            jobs: Vec::new(),
            exit_code: None,
            job_control: false,
            shell_pgid: 0,
            editor: None,
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
            exit_code: None,
            job_control: false,
            shell_pgid: self.shell_pgid,
            editor: None,
        }
    }

    pub fn run(mut self, args: &[String]) -> i32 {
        if args.len() >= 3 && args[1] == "-c" {
            self.positional = args[3..].to_vec();
            self.run_text(&args[2]);
            return self.exit_code.unwrap_or(self.last_status);
        }
        if args.len() >= 2 && !args[1].starts_with('-') {
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
        let nodes = match parse(&tokens) {
            Ok(nodes) => nodes,
            Err(ParseError::Incomplete) => return Err(true),
            Err(ParseError::Message(message)) => {
                eprintln!("sh: syntax error: {}", message);
                self.last_status = 2;
                return Err(false);
            }
        };
        self.exec_block(&nodes);
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
                    let flow = self.exec_block(condition);
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
                    let flow = self.exec_block(condition);
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
        for (connector, pipeline) in &and_or.items {
            match connector {
                Connector::AndThen if self.last_status != 0 => continue,
                Connector::OrElse if self.last_status == 0 => continue,
                _ => {}
            }
            let flow = self.exec_pipeline(pipeline);
            if flow != Flow::Normal {
                return flow;
            }
        }
        Flow::Normal
    }

    fn exec_pipeline(&mut self, pipeline: &Pipeline) -> Flow {
        let commands: Vec<Command> = pipeline
            .commands
            .iter()
            .map(|c| self.expand_command(c))
            .filter(|c| !c.words.is_empty() || c.stdout_file.is_some() || c.group.is_some())
            .collect();
        if commands.is_empty() {
            return Flow::Normal;
        }

        // A single builtin or function runs here so it can change our state.
        if commands.len() == 1 && !pipeline.background && commands[0].group.is_none() {
            let argv: Vec<String> = commands[0].words.iter().map(|(w, _)| w.clone()).collect();
            if !argv.is_empty() && (self.functions.contains_key(&argv[0]) || is_builtin(&argv[0]))
            {
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
                let argv: Vec<String> = command.words.iter().map(|(w, _)| w.clone()).collect();
                let mut child = self.fork_copy();
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

        if pipeline.background {
            let description: Vec<String> = commands
                .iter()
                .map(|c| c.words.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>().join(" "))
                .collect();
            if let Some(pid) = pids.last() {
                println!("[{}] {}", self.jobs.len() + 1, pid);
                self.jobs.push((*pid, description.join(" | ")));
            }
            self.last_status = 0;
            return Flow::Normal;
        }

        let mut status = 0;
        let mut interrupted = false;
        for pid in pids {
            let (rc, raw) = sys::wait4(pid, 0);
            if rc >= 0 {
                status = sys::exit_code_of(raw);
                if sys::signal_of(raw) == Some(sys::SIGINT) {
                    interrupted = true;
                }
            }
        }
        if self.job_control {
            sys::give_terminal_to(self.shell_pgid);
        }
        if interrupted {
            println!();
        }
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

    /// Run a builtin or shell function in this process, with redirections
    /// applied and then undone.
    fn run_local(&mut self, command: &Command, argv: &[String]) -> Flow {
        let saved_out = if command.stdout_file.is_some() { dup_fd(sys::STDOUT) } else { -1 };
        let saved_in = if command.stdin_file.is_some() || command.heredoc.is_some() {
            dup_fd(sys::STDIN)
        } else {
            -1
        };
        if apply_redirections(command).is_err() {
            self.last_status = 1;
            return Flow::Normal;
        }

        let flow = if let Some(body) = self.functions.get(&argv[0]).cloned() {
            let saved = std::mem::replace(&mut self.positional, argv[1..].to_vec());
            let flow = self.exec_block(&body);
            self.positional = saved;
            match flow {
                Flow::Return => Flow::Normal,
                other => other,
            }
        } else {
            self.run_builtin(argv)
        };

        let _ = std::io::stdout().flush();
        if saved_out >= 0 {
            sys::dup2(saved_out, sys::STDOUT);
            sys::close(saved_out);
        }
        if saved_in >= 0 {
            sys::dup2(saved_in, sys::STDIN);
            sys::close(saved_in);
        }
        flow
    }

    fn run_builtin(&mut self, argv: &[String]) -> Flow {
        self.last_status = match argv[0].as_str() {
            "cd" => {
                let target = argv
                    .get(1)
                    .cloned()
                    .unwrap_or_else(|| self.env.get("HOME").cloned().unwrap_or("/".into()));
                if sys::chdir(&target) < 0 {
                    eprintln!("cd: {}: no such directory", target);
                    1
                } else {
                    if let Ok(cwd) = std::env::current_dir() {
                        self.env.insert("PWD".into(), cwd.display().to_string());
                    }
                    0
                }
            }
            "exit" => {
                let code = argv.get(1).and_then(|a| a.parse().ok()).unwrap_or(self.last_status);
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
                for assignment in &argv[1..] {
                    if let Some((key, value)) = assignment.split_once('=') {
                        self.env.insert(key.to_string(), value.to_string());
                        std::env::set_var(key, value);
                    } else if let Some(value) = self.env.get(assignment).cloned() {
                        std::env::set_var(assignment, value);
                    }
                }
                0
            }
            "unset" => {
                for key in &argv[1..] {
                    self.env.remove(key);
                    std::env::remove_var(key);
                }
                0
            }
            "set" => {
                let mut keys: Vec<&String> = self.env.keys().collect();
                keys.sort();
                for key in keys {
                    println!("{}={}", key, self.env[key]);
                }
                0
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
                for (index, (pid, description)) in self.jobs.iter().enumerate() {
                    println!("[{}] {} {}", index + 1, pid, description);
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
                    std::env::set_var(key, value);
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
        const ENOENT: i64 = -2;
        const ENOEXEC: i64 = -8;
        const EACCES: i64 = -13;
        const EISDIR: i64 = -21;

        let envp: Vec<String> = self.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
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
            let (pid, status) = sys::wait4(-1, 1 /* WNOHANG */);
            if pid <= 0 {
                break;
            }
            if let Some(index) = self.jobs.iter().position(|(p, _)| *p == pid as i32) {
                let (_, description) = self.jobs.remove(index);
                println!("[done] {} ({})", description, sys::exit_code_of(status));
            }
        }
    }

    // -- expansion ---------------------------------------------------------

    fn expand_command(&mut self, command: &Command) -> Command {
        let mut out = Command {
            stdin_file: command.stdin_file.as_ref().map(|f| self.expand_text(f)),
            stdout_file: command.stdout_file.as_ref().map(|(f, a)| (self.expand_text(f), *a)),
            stderr_file: command.stderr_file.as_ref().map(|(f, a)| (self.expand_text(f), *a)),
            // An unquoted here-document delimiter means the body is expanded.
            heredoc: command.heredoc.as_ref().map(|(body, quoted)| {
                let text = if *quoted { body.clone() } else { self.expand_text(body) };
                (text, *quoted)
            }),
            group: command.group.clone(),
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
                out.push(chars[i + 1]);
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
                            out.push_str(&arithmetic(&resolved).to_string());
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
                        out.push_str("sh");
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
                        let mut name = String::new();
                        while i < chars.len() && chars[i] != '}' {
                            name.push(chars[i]);
                            i += 1;
                        }
                        i += 1;
                        let value = self.lookup(&name).to_string();
                        out.push_str(&value);
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
    "source", "help", "history", ":",
];

fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name) || name.contains('=')
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
    if let Some((path, append)) = &command.stdout_file {
        let flags =
            sys::O_WRONLY | sys::O_CREAT | if *append { sys::O_APPEND } else { sys::O_TRUNC };
        let fd = sys::open(path, flags, 0o644);
        if fd < 0 {
            eprintln!("sh: {}: cannot create", path);
            return Err(());
        }
        sys::dup2(fd as i32, sys::STDOUT);
        sys::close(fd as i32);
    }
    if let Some((path, append)) = &command.stderr_file {
        let flags =
            sys::O_WRONLY | sys::O_CREAT | if *append { sys::O_APPEND } else { sys::O_TRUNC };
        let fd = sys::open(path, flags, 0o644);
        if fd < 0 {
            return Err(());
        }
        sys::dup2(fd as i32, sys::STDERR);
        sys::close(fd as i32);
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
fn arithmetic(expression: &str) -> i64 {
    let tokens: Vec<char> = expression.chars().filter(|c| !c.is_whitespace()).collect();
    let mut position = 0;
    parse_sum(&tokens, &mut position)
}

fn parse_sum(tokens: &[char], position: &mut usize) -> i64 {
    let mut value = parse_product(tokens, position);
    while *position < tokens.len() {
        match tokens[*position] {
            '+' => {
                *position += 1;
                value += parse_product(tokens, position);
            }
            '-' => {
                *position += 1;
                value -= parse_product(tokens, position);
            }
            _ => break,
        }
    }
    value
}

fn parse_product(tokens: &[char], position: &mut usize) -> i64 {
    let mut value = parse_atom(tokens, position);
    while *position < tokens.len() {
        let operator = tokens[*position];
        if !matches!(operator, '*' | '/' | '%') {
            break;
        }
        *position += 1;
        let operand = parse_atom(tokens, position);
        value = match operator {
            '*' => value * operand,
            '/' if operand != 0 => value / operand,
            '%' if operand != 0 => value % operand,
            _ => value,
        };
    }
    value
}

fn parse_atom(tokens: &[char], position: &mut usize) -> i64 {
    if *position >= tokens.len() {
        return 0;
    }
    match tokens[*position] {
        '(' => {
            *position += 1;
            let value = parse_sum(tokens, position);
            if tokens.get(*position) == Some(&')') {
                *position += 1;
            }
            value
        }
        '-' => {
            *position += 1;
            -parse_atom(tokens, position)
        }
        _ => {
            let start = *position;
            while *position < tokens.len() && tokens[*position].is_ascii_digit() {
                *position += 1;
            }
            tokens[start..*position].iter().collect::<String>().parse().unwrap_or(0)
        }
    }
}

/// Expand a `*`/`?` pattern against the filesystem.
fn glob(pattern: &str) -> Option<Vec<String>> {
    let (dir, file_pattern) = match pattern.rfind('/') {
        Some(index) => (&pattern[..index + 1], &pattern[index + 1..]),
        None => ("", pattern),
    };
    if !file_pattern.contains('*') && !file_pattern.contains('?') {
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

fn matches_pattern(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ni, mut pi) = (0usize, 0usize);
    let (mut star, mut backtrack) = (usize::MAX, 0usize);

    while ni < n.len() {
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
