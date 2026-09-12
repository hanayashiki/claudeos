//! A small backtracking regular expression matcher for grep.
//!
//! Supports anchors, `.`, character classes, and the `*` repeat in basic mode;
//! extended mode adds `+`, `?`, alternation and groups. That is the subset
//! people actually type at a grep prompt.

#[derive(Debug, Clone)]
enum Atom {
    Char(char),
    Any,
    Class { ranges: Vec<(char, char)>, negated: bool },
    Group(Vec<Branch>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Repeat {
    One,
    Star,
    Plus,
    Optional,
}

#[derive(Debug, Clone)]
struct Piece {
    atom: Atom,
    repeat: Repeat,
}

#[derive(Debug, Clone)]
struct Branch {
    pieces: Vec<Piece>,
    anchored_start: bool,
    anchored_end: bool,
}

pub struct Regex {
    branches: Vec<Branch>,
}

struct Parser<'a> {
    chars: &'a [char],
    position: usize,
    extended: bool,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn parse_alternation(&mut self, nested: bool) -> Vec<Branch> {
        let mut branches = Vec::new();
        loop {
            branches.push(self.parse_branch(nested));
            if self.extended && self.peek() == Some('|') {
                self.position += 1;
                continue;
            }
            break;
        }
        branches
    }

    fn parse_branch(&mut self, nested: bool) -> Branch {
        let mut pieces = Vec::new();
        let mut anchored_start = false;
        let mut anchored_end = false;

        if self.peek() == Some('^') {
            anchored_start = true;
            self.position += 1;
        }

        while let Some(c) = self.peek() {
            if self.extended && c == '|' {
                break;
            }
            if nested && c == ')' {
                break;
            }
            if c == '$' && self.at_branch_end(nested) {
                anchored_end = true;
                self.position += 1;
                break;
            }

            let atom = match c {
                '\\' => {
                    self.position += 1;
                    match self.peek() {
                        Some(escaped) => {
                            self.position += 1;
                            Atom::Char(escaped)
                        }
                        None => Atom::Char('\\'),
                    }
                }
                '.' => {
                    self.position += 1;
                    Atom::Any
                }
                '[' => {
                    self.position += 1;
                    self.parse_class()
                }
                '(' if self.extended => {
                    self.position += 1;
                    let inner = self.parse_alternation(true);
                    if self.peek() == Some(')') {
                        self.position += 1;
                    }
                    Atom::Group(inner)
                }
                other => {
                    self.position += 1;
                    Atom::Char(other)
                }
            };

            let repeat = match self.peek() {
                Some('*') => {
                    self.position += 1;
                    Repeat::Star
                }
                Some('+') if self.extended => {
                    self.position += 1;
                    Repeat::Plus
                }
                Some('?') if self.extended => {
                    self.position += 1;
                    Repeat::Optional
                }
                _ => Repeat::One,
            };
            pieces.push(Piece { atom, repeat });
        }

        Branch { pieces, anchored_start, anchored_end }
    }

    /// `$` is an anchor only at the end of the pattern or of a branch.
    fn at_branch_end(&self, nested: bool) -> bool {
        match self.chars.get(self.position + 1) {
            None => true,
            Some('|') if self.extended => true,
            Some(')') if nested => true,
            _ => false,
        }
    }

    fn parse_class(&mut self) -> Atom {
        let mut ranges = Vec::new();
        let mut negated = false;
        if self.peek() == Some('^') {
            negated = true;
            self.position += 1;
        }
        // A ']' first is a literal.
        if self.peek() == Some(']') {
            ranges.push((']', ']'));
            self.position += 1;
        }
        while let Some(c) = self.peek() {
            if c == ']' {
                self.position += 1;
                break;
            }
            self.position += 1;
            let start = if c == '\\' {
                match self.peek() {
                    Some(escaped) => {
                        self.position += 1;
                        escaped
                    }
                    None => '\\',
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.position + 1) != Some(&']') {
                self.position += 1;
                if let Some(end) = self.peek() {
                    self.position += 1;
                    ranges.push((start, end));
                    continue;
                }
            }
            ranges.push((start, start));
        }
        Atom::Class { ranges, negated }
    }
}

impl Regex {
    /// Compile `pattern`. `extended` enables `+`, `?`, `|` and groups.
    pub fn new(pattern: &str, extended: bool) -> Regex {
        let chars: Vec<char> = pattern.chars().collect();
        let mut parser = Parser { chars: &chars, position: 0, extended };
        Regex { branches: parser.parse_alternation(false) }
    }

    /// A pattern to be taken literally, for grep -F.
    pub fn literal(pattern: &str) -> Regex {
        let pieces = pattern
            .chars()
            .map(|c| Piece { atom: Atom::Char(c), repeat: Repeat::One })
            .collect();
        Regex {
            branches: vec![Branch { pieces, anchored_start: false, anchored_end: false }],
        }
    }

    /// The first place `text` matches, as a half-open range of character
    /// positions. Leftmost first, and greedy from there.
    pub fn find(&self, text: &[char], from: usize) -> Option<(usize, usize)> {
        for start in from..=text.len() {
            for branch in &self.branches {
                if branch.anchored_start && start != 0 {
                    continue;
                }
                if branch.anchored_end {
                    // `$` pins the end, so a match that reaches it ends there.
                    if match_branch(&branch.pieces, text, start, true) {
                        return Some((start, text.len()));
                    }
                } else if let Some(end) = match_pieces(&branch.pieces, text, start) {
                    return Some((start, end));
                }
            }
        }
        None
    }

    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        for branch in &self.branches {
            let starts: Vec<usize> =
                if branch.anchored_start { vec![0] } else { (0..=chars.len()).collect() };
            for start in starts {
                if match_branch(&branch.pieces, &chars, start, branch.anchored_end) {
                    return true;
                }
            }
        }
        false
    }
}

/// Match `pieces` starting at `pos`, requiring the end of the text when the
/// branch was anchored with `$`.
fn match_branch(pieces: &[Piece], text: &[char], pos: usize, to_end: bool) -> bool {
    if pieces.is_empty() {
        return !to_end || pos == text.len();
    }
    let piece = &pieces[0];
    let rest = &pieces[1..];

    // A group is spliced into the sequence so the continuation can backtrack
    // into a different alternative.
    if let (Atom::Group(branches), Repeat::One) = (&piece.atom, piece.repeat) {
        for branch in branches {
            let mut combined = branch.pieces.clone();
            combined.extend_from_slice(rest);
            if match_branch(&combined, text, pos, to_end) {
                return true;
            }
        }
        return false;
    }

    // Longest first: a repeat is greedy, but gives ground on backtracking.
    for next in candidates(piece, text, pos).into_iter().rev() {
        if match_branch(rest, text, next, to_end) {
            return true;
        }
    }
    false
}

/// Every position this piece could leave the cursor at, shortest first.
fn candidates(piece: &Piece, text: &[char], pos: usize) -> Vec<usize> {
    match piece.repeat {
        Repeat::One => match_atom(&piece.atom, text, pos).into_iter().collect(),
        Repeat::Optional => {
            let mut out = vec![pos];
            out.extend(match_atom(&piece.atom, text, pos));
            out
        }
        Repeat::Star | Repeat::Plus => {
            let mut out = Vec::new();
            if piece.repeat == Repeat::Star {
                out.push(pos);
            }
            let mut cursor = pos;
            loop {
                match match_atom(&piece.atom, text, cursor) {
                    Some(next) if next > cursor => {
                        cursor = next;
                        out.push(cursor);
                    }
                    _ => break,
                }
            }
            out
        }
    }
}

fn match_pieces(pieces: &[Piece], text: &[char], pos: usize) -> Option<usize> {
    if pieces.is_empty() {
        return Some(pos);
    }
    let piece = &pieces[0];
    let rest = &pieces[1..];

    // A group has to be spliced in so the continuation can backtrack into it.
    if let (Atom::Group(branches), Repeat::One) = (&piece.atom, piece.repeat) {
        for branch in branches {
            let mut combined = branch.pieces.clone();
            combined.extend_from_slice(rest);
            if let Some(end) = match_pieces(&combined, text, pos) {
                return Some(end);
            }
        }
        return None;
    }

    // Longest first, which is what a greedy repeat means.
    for next in candidates(piece, text, pos).into_iter().rev() {
        if let Some(end) = match_pieces(rest, text, next) {
            return Some(end);
        }
    }
    None
}

fn match_atom(atom: &Atom, text: &[char], pos: usize) -> Option<usize> {
    match atom {
        Atom::Char(expected) => {
            if text.get(pos) == Some(expected) {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Any => {
            if pos < text.len() {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Class { ranges, negated } => {
            let c = *text.get(pos)?;
            let inside = ranges.iter().any(|(low, high)| c >= *low && c <= *high);
            if inside != *negated {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Group(branches) => {
            for branch in branches {
                if let Some(end) = match_pieces(&branch.pieces, text, pos) {
                    return Some(end);
                }
            }
            None
        }
    }
}
