//! Semantic versions and version constraints.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: None,
        }
    }

    pub fn with_pre(mut self, pre: impl Into<String>) -> Self {
        self.pre = Some(pre.into());
        self
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.major.cmp(&other.major) {
            Ordering::Equal => match self.minor.cmp(&other.minor) {
                Ordering::Equal => match self.patch.cmp(&other.patch) {
                    Ordering::Equal => match (&self.pre, &other.pre) {
                        (None, None) => Ordering::Equal,
                        (Some(_), None) => Ordering::Less, // pre-release is lower precedence
                        (None, Some(_)) => Ordering::Greater,
                        (Some(a), Some(b)) => compare_pre(a, b),
                    },
                    other_ord => other_ord,
                },
                other_ord => other_ord,
            },
            other_ord => other_ord,
        }
    }
}

/// Semver precedence of two prerelease strings: dot-separated identifiers,
/// numeric ones compared as numbers and ranked below alphanumeric ones, and
/// a shorter list ranked first when it is a prefix of the longer one.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{}", pre)?;
        }
        Ok(())
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let (num_part, pre_part) = if let Some((n, p)) = trimmed.split_once('-') {
            (n, Some(p.to_string()))
        } else {
            (trimmed, None)
        };

        let parts: Vec<&str> = num_part.split('.').collect();
        if parts.len() > 3 || parts.is_empty() {
            return Err(format!("invalid semver string '{}'", s));
        }

        let major = parts[0]
            .parse::<u64>()
            .map_err(|_| format!("invalid major in '{}'", s))?;
        let minor = if parts.len() > 1 {
            parts[1]
                .parse::<u64>()
                .map_err(|_| format!("invalid minor in '{}'", s))?
        } else {
            0
        };
        let patch = if parts.len() > 2 {
            parts[2]
                .parse::<u64>()
                .map_err(|_| format!("invalid patch in '{}'", s))?
        } else {
            0
        };

        Ok(Self {
            major,
            minor,
            patch,
            pre: pre_part,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionReq {
    Any,
    Exact(Version),
    Caret(Version),
    Tilde(Version),
    GreaterOrEqual(Version),
    Greater(Version),
    LessOrEqual(Version),
    LessThan(Version),
    /// Comma-separated requirements that must all hold: `>=1.2, <2`.
    All(Vec<VersionReq>),
}

impl VersionReq {
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed == "*" || trimmed.is_empty() {
            return Ok(Self::Any);
        }

        if trimmed.contains(',') {
            let parts = trimmed
                .split(',')
                .map(Self::parse)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::All(parts));
        }

        if let Some(rest) = trimmed.strip_prefix('^') {
            let ver = Version::from_str(rest)?;
            return Ok(Self::Caret(ver));
        }

        if let Some(rest) = trimmed.strip_prefix('~') {
            let ver = Version::from_str(rest)?;
            return Ok(Self::Tilde(ver));
        }

        if let Some(rest) = trimmed.strip_prefix(">=") {
            let ver = Version::from_str(rest)?;
            return Ok(Self::GreaterOrEqual(ver));
        }

        if let Some(rest) = trimmed.strip_prefix("<=") {
            let ver = Version::from_str(rest)?;
            return Ok(Self::LessOrEqual(ver));
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let ver = Version::from_str(rest)?;
            return Ok(Self::Greater(ver));
        }

        if let Some(rest) = trimmed.strip_prefix('<') {
            let ver = Version::from_str(rest)?;
            return Ok(Self::LessThan(ver));
        }

        if let Some(rest) = trimmed.strip_prefix('=') {
            let ver = Version::from_str(rest)?;
            return Ok(Self::Exact(ver));
        }

        // Default bare version "1.2.3" treated as Caret "^1.2.3" like npm/cargo
        let ver = Version::from_str(trimmed)?;
        Ok(Self::Caret(ver))
    }

    /// Whether `ver` satisfies the requirement. As in Cargo and npm, a
    /// prerelease only satisfies a requirement that names a prerelease of
    /// the same `major.minor.patch`, so `^1.0.0` never picks `1.1.0-beta`.
    pub fn matches(&self, ver: &Version) -> bool {
        if ver.pre.is_some() && !self.allows_pre_of(ver) {
            return false;
        }
        self.matches_ignoring_pre(ver)
    }

    fn allows_pre_of(&self, ver: &Version) -> bool {
        let same = |v: &Version| {
            v.pre.is_some() && v.major == ver.major && v.minor == ver.minor && v.patch == ver.patch
        };
        match self {
            Self::Any => false,
            Self::Exact(v)
            | Self::Caret(v)
            | Self::Tilde(v)
            | Self::GreaterOrEqual(v)
            | Self::Greater(v)
            | Self::LessOrEqual(v)
            | Self::LessThan(v) => same(v),
            Self::All(parts) => parts.iter().any(|part| part.allows_pre_of(ver)),
        }
    }

    fn matches_ignoring_pre(&self, ver: &Version) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(v) => ver == v,
            Self::GreaterOrEqual(v) => ver >= v,
            Self::Greater(v) => ver > v,
            Self::LessOrEqual(v) => ver <= v,
            Self::LessThan(v) => ver < v,
            Self::All(parts) => parts.iter().all(|part| part.matches_ignoring_pre(ver)),
            Self::Caret(v) => {
                if ver < v {
                    return false;
                }
                if v.major > 0 {
                    ver.major == v.major
                } else if v.minor > 0 {
                    ver.major == 0 && ver.minor == v.minor
                } else {
                    ver.major == 0 && ver.minor == 0 && ver.patch == v.patch
                }
            }
            Self::Tilde(v) => {
                if ver < v {
                    return false;
                }
                ver.major == v.major && ver.minor == v.minor
            }
        }
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => write!(f, "*"),
            Self::Exact(v) => write!(f, "={}", v),
            Self::Caret(v) => write!(f, "^{}", v),
            Self::Tilde(v) => write!(f, "~{}", v),
            Self::GreaterOrEqual(v) => write!(f, ">={}", v),
            Self::Greater(v) => write!(f, ">{}", v),
            Self::LessOrEqual(v) => write!(f, "<={}", v),
            Self::LessThan(v) => write!(f, "<{}", v),
            Self::All(parts) => {
                let parts: Vec<String> = parts.iter().map(ToString::to_string).collect();
                write!(f, "{}", parts.join(", "))
            }
        }
    }
}
