//! A small fuzzy matcher for pickers.
//!
//! The query matches when its characters appear in order in the candidate,
//! ignoring case. The score rewards consecutive runs and matches at word
//! starts (after `/`, `-`, `_`, `.` or a space) and penalizes gaps, so `api`
//! ranks `bob/api` above `bob/rapid`. It allocates nothing per
//! candidate, which keeps filtering thousands of branches per keystroke well
//! under a frame.

/// Bonus for a match at the start of the candidate or of a word.
const WORD_START_BONUS: i32 = 8;
/// Bonus for a match right after the previous one.
const CONSECUTIVE_BONUS: i32 = 5;
/// Base score per matched character.
const MATCH_SCORE: i32 = 1;
/// Penalty per skipped candidate character between two matches.
const GAP_PENALTY: i32 = 1;
/// Largest penalty for a single gap, so long names are not over-punished.
const MAX_GAP_PENALTY: i32 = 6;

/// A lowercased query, prepared once per keystroke.
#[derive(Clone, Debug, Default)]
pub struct FuzzyQuery {
    chars: Vec<char>,
}

impl FuzzyQuery {
    /// Prepares `query`; whitespace is ignored.
    pub fn new(query: &str) -> Self {
        Self {
            chars: query
                .chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase)
                .collect(),
        }
    }

    /// Whether the query matches everything.
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Scores `candidate`; `None` when it does not match. Higher is better.
    pub fn score(&self, candidate: &str) -> Option<i32> {
        if self.chars.is_empty() {
            return Some(0);
        }
        let mut wanted = self.chars.iter().copied().peekable();
        let mut score = 0;
        let mut previous: Option<char> = None;
        let mut last_match: Option<usize> = None;
        for (index, c) in candidate.chars().enumerate() {
            let Some(&next) = wanted.peek() else {
                break;
            };
            if c.to_lowercase().eq(std::iter::once(next)) {
                score += MATCH_SCORE;
                if previous.is_none_or(is_separator) {
                    score += WORD_START_BONUS;
                }
                match last_match {
                    Some(last) if last + 1 == index => score += CONSECUTIVE_BONUS,
                    Some(last) => {
                        score -= ((index - last - 1) as i32 * GAP_PENALTY).min(MAX_GAP_PENALTY)
                    }
                    None => {}
                }
                last_match = Some(index);
                wanted.next();
            }
            previous = Some(c);
        }
        wanted.peek().is_none().then_some(score)
    }
}

fn is_separator(c: char) -> bool {
    matches!(c, '/' | '-' | '_' | '.' | ' ')
}
