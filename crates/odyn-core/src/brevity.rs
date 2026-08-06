//! Brevity mode: a per-level style directive injected under `## Style`.
//!
//! The native form of the caveman idea — compress the answer, never the
//! substance. Each directive is hand-written, ≤80 tokens by the same chars/4
//! heuristic the brain uses, and snapshot-tested so it cannot drift silently.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brevity {
    #[default]
    Off,
    Lite,
    Full,
    Ultra,
}

const LITE: &str = "Trim the filler: no preamble, hedging, apologies, or restating \
of the question. Full sentences are fine; keep them lean and complete. Never alter, \
truncate, or restyle code blocks, shell commands, file paths, identifiers, or error \
messages — reproduce those byte-exact.";

const FULL: &str = "Prefer tight fragments over full sentences. Cut every filler \
word: no preamble, hedging, transitions, or summary closings. Substance only. Never \
alter, truncate, or restyle code blocks, shell commands, file paths, identifiers, \
or error messages — reproduce those byte-exact.";

const ULTRA: &str = "Minimum viable words. Telegraphic fragments. One line where \
one line works. No filler of any kind. Never alter, truncate, or restyle code \
blocks, shell commands, file paths, identifiers, or error messages — reproduce \
those byte-exact.";

impl Brevity {
    pub const ALL: [Brevity; 4] = [Brevity::Off, Brevity::Lite, Brevity::Full, Brevity::Ultra];

    /// The directive injected under `## Style`; `Off` injects nothing.
    pub fn directive(self) -> Option<&'static str> {
        match self {
            Brevity::Off => None,
            Brevity::Lite => Some(LITE),
            Brevity::Full => Some(FULL),
            Brevity::Ultra => Some(ULTRA),
        }
    }
}

impl fmt::Display for Brevity {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Brevity::Off => "off",
            Brevity::Lite => "lite",
            Brevity::Full => "full",
            Brevity::Ultra => "ultra",
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown brevity level `{0}`; use off, lite, full, or ultra")]
pub struct BadBrevity(String);

impl FromStr for Brevity {
    type Err = BadBrevity;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "off" => Ok(Brevity::Off),
            "lite" => Ok(Brevity::Lite),
            "full" => Ok(Brevity::Full),
            "ultra" => Ok(Brevity::Ultra),
            other => Err(BadBrevity(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot: any edit to a directive has to be made twice, on purpose.
    #[test]
    fn the_directives_are_exactly_these_words() {
        assert_eq!(
            Brevity::Lite.directive().expect("lite"),
            "Trim the filler: no preamble, hedging, apologies, or restating of the \
             question. Full sentences are fine; keep them lean and complete. Never \
             alter, truncate, or restyle code blocks, shell commands, file paths, \
             identifiers, or error messages — reproduce those byte-exact."
        );
        assert_eq!(
            Brevity::Full.directive().expect("full"),
            "Prefer tight fragments over full sentences. Cut every filler word: no \
             preamble, hedging, transitions, or summary closings. Substance only. \
             Never alter, truncate, or restyle code blocks, shell commands, file \
             paths, identifiers, or error messages — reproduce those byte-exact."
        );
        assert_eq!(
            Brevity::Ultra.directive().expect("ultra"),
            "Minimum viable words. Telegraphic fragments. One line where one line \
             works. No filler of any kind. Never alter, truncate, or restyle code \
             blocks, shell commands, file paths, identifiers, or error messages — \
             reproduce those byte-exact."
        );
        assert_eq!(Brevity::Off.directive(), None);
    }

    #[test]
    fn every_directive_fits_the_eighty_token_budget() {
        for level in Brevity::ALL {
            let Some(directive) = level.directive() else {
                continue;
            };
            let tokens = directive.chars().count().div_ceil(4);
            assert!(tokens <= 80, "{level}: {tokens} tokens");
            assert!(
                directive.contains("byte-exact"),
                "{level} must state the invariant"
            );
        }
    }

    #[test]
    fn levels_round_trip_through_their_names() {
        for level in Brevity::ALL {
            assert_eq!(level.to_string().parse::<Brevity>().expect("parse"), level);
        }
        assert!("caveman".parse::<Brevity>().is_err());
        assert_eq!(Brevity::default(), Brevity::Off);
        assert_eq!(
            serde_json::to_string(&Brevity::Ultra).expect("serialize"),
            "\"ultra\""
        );
    }
}
