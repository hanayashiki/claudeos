//! A small backtracking regular expression matcher for grep and sed.
//!
//! Supports anchors, `.`, character classes, and the `*` repeat. Groups are
//! written `\(...\)` in basic mode and `(...)` in extended mode, which also
//! gives `+`, `?` and bare alternation; basic mode spells alternation `\|`.
//! A group's text can be referred to afterwards, which is what a sed
//! replacement means by `\1`.

#[derive(Debug, Clone)]
enum Atom {
    Char(char),
    Any,
    Class { ranges: Vec<(char, char)>, negated: bool },
    /// A group and the number it answers to.
    Group(Vec<Branch>, usize),
    /// Matches nothing; records where a group started or ended.
    Mark(usize, bool),
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
    /// How many groups the pattern has, so a caller knows how much room to
    /// leave for what they matched.
    groups: usize,
}

/// Where each group matched, by number. Index 0 is unused; a group that did
/// not take part is None.
pub type Captures = Vec<Option<(usize, usize)>>;

struct Parser<'a> {
    chars: &'a [char],
    position: usize,
    extended: bool,
    /// Groups opened so far, which is how each one gets its number.
    groups: usize,
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
            if !self.extended
                && self.peek() == Some('\\')
                && self.chars.get(self.position + 1) == Some(&'|')
            {
                self.position += 2;
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

            // In basic mode a group is spelled with backslashes, and so is
            // alternation, so both have to be looked at before the escape is
            // treated as a literal character.
            if !self.extended && c == '\\' {
                match self.chars.get(self.position + 1) {
                    Some(')') if nested => break,
                    Some('|') => break,
                    _ => {}
                }
            }

            let atom = match c {
                '\\' if !self.extended && self.chars.get(self.position + 1) == Some(&'(') => {
                    self.position += 2;
                    self.groups += 1;
                    let index = self.groups;
                    let inner = self.parse_alternation(true);
                    if self.chars.get(self.position) == Some(&'\\')
                        && self.chars.get(self.position + 1) == Some(&')')
                    {
                        self.position += 2;
                    }
                    Atom::Group(inner, index)
                }
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
                    self.groups += 1;
                    let index = self.groups;
                    let inner = self.parse_alternation(true);
                    if self.peek() == Some(')') {
                        self.position += 1;
                    }
                    Atom::Group(inner, index)
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
                // GNU basic mode spells the same two with a backslash.
                Some('\\')
                    if !self.extended
                        && matches!(self.chars.get(self.position + 1), Some('+') | Some('?')) =>
                {
                    let which = self.chars[self.position + 1];
                    self.position += 2;
                    if which == '+' {
                        Repeat::Plus
                    } else {
                        Repeat::Optional
                    }
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
        let mut parser = Parser { chars: &chars, position: 0, extended, groups: 0 };
        let branches = parser.parse_alternation(false);
        Regex { branches, groups: parser.groups }
    }

    /// A pattern to be taken literally, for grep -F.
    pub fn literal(pattern: &str) -> Regex {
        let pieces = pattern
            .chars()
            .map(|c| Piece { atom: Atom::Char(c), repeat: Repeat::One })
            .collect();
        Regex {
            branches: vec![Branch { pieces, anchored_start: false, anchored_end: false }],
            groups: 0,
        }
    }

    /// The first place `text` matches, as a half-open range of character
    /// positions. Leftmost first, and greedy from there.
    pub fn find(&self, text: &[char], from: usize) -> Option<(usize, usize)> {
        let mut caps = Vec::new();
        self.find_captures(text, from, &mut caps)
    }

    /// As `find`, filling `caps` with where each group matched.
    pub fn find_captures(
        &self,
        text: &[char],
        from: usize,
        caps: &mut Captures,
    ) -> Option<(usize, usize)> {
        for start in from..=text.len() {
            for branch in &self.branches {
                if branch.anchored_start && start != 0 {
                    continue;
                }
                caps.clear();
                caps.resize(self.groups + 1, None);
                if branch.anchored_end {
                    // `$` pins the end, so a match that reaches it ends there.
                    if match_branch_caps(&branch.pieces, text, start, true, caps) {
                        return Some((start, text.len()));
                    }
                } else if let Some(end) = match_pieces_caps(&branch.pieces, text, start, caps) {
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
    let mut caps: Captures = Vec::new();
    match_branch_caps(pieces, text, pos, to_end, &mut caps)
}

fn match_branch_caps(
    pieces: &[Piece],
    text: &[char],
    pos: usize,
    to_end: bool,
    caps: &mut Captures,
) -> bool {
    if pieces.is_empty() {
        return !to_end || pos == text.len();
    }
    let piece = &pieces[0];
    let rest = &pieces[1..];

    if let (Atom::Mark(index, open), Repeat::One) = (&piece.atom, piece.repeat) {
        let saved = caps.get(*index).copied().flatten();
        note(caps, *index, *open, pos);
        if match_branch_caps(rest, text, pos, to_end, caps) {
            return true;
        }
        if let Some(slot) = caps.get_mut(*index) {
            *slot = saved;
        }
        return false;
    }

    // A group is spliced into the sequence so the continuation can backtrack
    // into a different alternative, with a marker at each end to record what
    // it covered.
    if let (Atom::Group(branches, index), Repeat::One) = (&piece.atom, piece.repeat) {
        for branch in branches {
            let mut combined = vec![marker(*index, true)];
            combined.extend(branch.pieces.clone());
            combined.push(marker(*index, false));
            combined.extend_from_slice(rest);
            if match_branch_caps(&combined, text, pos, to_end, caps) {
                return true;
            }
        }
        return false;
    }

    // Longest first: a repeat is greedy, but gives ground on backtracking.
    for next in candidates(piece, text, pos).into_iter().rev() {
        if match_branch_caps(rest, text, next, to_end, caps) {
            return true;
        }
    }
    false
}

fn marker(index: usize, open: bool) -> Piece {
    Piece { atom: Atom::Mark(index, open), repeat: Repeat::One }
}

/// Record where a group opened or closed.
fn note(caps: &mut Captures, index: usize, open: bool, pos: usize) {
    if caps.len() <= index {
        caps.resize(index + 1, None);
    }
    if open {
        caps[index] = Some((pos, pos));
    } else if let Some((start, _)) = caps[index] {
        caps[index] = Some((start, pos));
    }
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
    let mut caps: Captures = Vec::new();
    match_pieces_caps(pieces, text, pos, &mut caps)
}

fn match_pieces_caps(
    pieces: &[Piece],
    text: &[char],
    pos: usize,
    caps: &mut Captures,
) -> Option<usize> {
    if pieces.is_empty() {
        return Some(pos);
    }
    let piece = &pieces[0];
    let rest = &pieces[1..];

    if let (Atom::Mark(index, open), Repeat::One) = (&piece.atom, piece.repeat) {
        let saved = caps.get(*index).copied().flatten();
        note(caps, *index, *open, pos);
        if let Some(end) = match_pieces_caps(rest, text, pos, caps) {
            return Some(end);
        }
        if let Some(slot) = caps.get_mut(*index) {
            *slot = saved;
        }
        return None;
    }

    // A group has to be spliced in so the continuation can backtrack into it.
    if let (Atom::Group(branches, index), Repeat::One) = (&piece.atom, piece.repeat) {
        for branch in branches {
            let mut combined = vec![marker(*index, true)];
            combined.extend(branch.pieces.clone());
            combined.push(marker(*index, false));
            combined.extend_from_slice(rest);
            if let Some(end) = match_pieces_caps(&combined, text, pos, caps) {
                return Some(end);
            }
        }
        return None;
    }

    // Longest first, which is what a greedy repeat means.
    for next in candidates(piece, text, pos).into_iter().rev() {
        if let Some(end) = match_pieces_caps(rest, text, next, caps) {
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
        Atom::Group(branches, _) => {
            for branch in branches {
                if let Some(end) = match_pieces(&branch.pieces, text, pos) {
                    return Some(end);
                }
            }
            None
        }
        // A marker consumes nothing; the recording happens where the
        // continuation is known.
        Atom::Mark(..) => Some(pos),
    }
}
