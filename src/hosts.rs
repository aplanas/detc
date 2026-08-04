//! The hosts that one run of [`detctl`](crate::detctl) reaches.
//!
//! A fleet is named the way it is named everywhere else: a host, a group of
//! them, a run counted between two ends, a shell pattern, and a `!` that takes
//! some back out.  The groups live in one file of whoever is typing:
//!
//! ```yaml
//! dmz:
//!   - web1
//!   - web2.example
//! stage:
//!   - stage-web1
//!   - stage-web2
//! lab:
//!   - lab[01:12]     # a run of machines, counted out
//! web:
//!   - dmz            # a group may name another
//!   - stage-web*     # and a pattern gathers the ones named above
//! ```
//!
//! A pattern in the file selects among the hosts the file itself names and
//! never introduces one, so `stage-web*` reaches those two machines only
//! because `stage` lists them.  What it is for is a tag that cuts across the
//! groups; where a group would do, the group is shorter and does not go quiet
//! when somebody renames a machine.
//!
//! A range is the other way round: `lab[01:12]` counts out `lab01` to `lab12`,
//! which are machines that are written down nowhere.  The two ends are the whole
//! of what it reaches, so a run is the size it was typed as and never the size
//! the network happens to have.  It reads the same on the command line, counts
//! letters as well as numbers (`rack[a:f]`), and counts an address like a name:
//! `192.168.1.[10:250]`.
//!
//! It says which hosts there are and nothing about how to reach them: a port,
//! a user, a key or a jump host is what `~/.ssh/config` is for, and `detctl`
//! does not offer a second place to write them down.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};

use glob::Pattern;
use log::debug;

use detc::var::Variables;
use detc::{Result, err};

/// Where the inventory is, under the configuration directory of the user.
const INVENTORY: &str = "detc/hosts.yaml";

/// The variable that names another inventory, for a run that is not driven from
/// the home directory of anybody.
const ENV: &str = "DETC_HOSTS";

/// What a shell pattern is written with.  A name that holds none of these is a
/// name, and is never matched against anything.  The brackets are also what a
/// range counts between, and [`range`] is what tells the two apart.
const WILDCARDS: [char; 3] = ['*', '?', '['];

/// What a range steps through between its two ends.
#[derive(Clone, Copy)]
enum Count {
    /// Numbers, each written with at least `width` digits.
    Numbers { low: u64, high: u64, width: usize },

    /// Letters, of the case they were written in.
    Letters { low: u8, high: u8 },
}

impl Count {
    /// Whether the count runs the way it was written.  One that runs the other
    /// way steps through nothing, and is refused where it is asked for.
    fn ascends(self) -> bool {
        match self {
            Count::Numbers { low, high, .. } => low <= high,
            Count::Letters { low, high } => low <= high,
        }
    }

    /// Every step from one end to the other, written the way the ends were.
    fn steps(self) -> impl Iterator<Item = String> {
        let (low, high) = match self {
            Count::Numbers { low, high, .. } => (low, high),
            Count::Letters { low, high } => (u64::from(low), u64::from(high)),
        };

        (low..=high).map(move |step| match self {
            Count::Numbers { width, .. } => format!("{step:0width$}"),
            Count::Letters { .. } => char::from(step as u8).to_string(),
        })
    }
}

/// A run of machines that are named alike but for one part that counts:
/// the text before it, the text after it, and the count between them.
///
/// `web[01:12]` is one.  Unlike a pattern it names machines that are written
/// down nowhere, which is what makes it worth having: a rack, a row or a run of
/// addresses is asked for by the two ends it was typed with.
struct Range<'a> {
    prefix: &'a str,
    suffix: &'a str,
    count: Count,
}

impl Range<'_> {
    /// Every name the range stands for.
    fn names(&self) -> impl Iterator<Item = String> {
        let (prefix, suffix) = (self.prefix, self.suffix);

        self.count
            .steps()
            .map(move |step| format!("{prefix}{step}{suffix}"))
    }
}

/// The count that lies between two ends, if the two are ends of one.
fn count(low: &str, high: &str) -> Option<Count> {
    if low.is_empty() || high.is_empty() {
        return None;
    }

    if low
        .bytes()
        .chain(high.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return Some(Count::Numbers {
            low: low.parse().ok()?,
            high: high.parse().ok()?,

            // A zero written in front of the lower end is how wide every number
            // of the run is written, and without one it is as wide as it needs
            // to be: `[01:12]` counts `01`, and `[1:12]` counts `1`
            width: match low.starts_with('0') {
                true => low.len(),
                false => 1,
            },
        });
    }

    // A letter counts to a letter of its own case, because what lies between
    // the two cases is punctuation and no machine is named after it
    match (low.as_bytes(), high.as_bytes()) {
        ([low], [high])
            if low.is_ascii_lowercase() && high.is_ascii_lowercase()
                || low.is_ascii_uppercase() && high.is_ascii_uppercase() =>
        {
            Some(Count::Letters {
                low: *low,
                high: *high,
            })
        }
        _ => None,
    }
}

/// The range that a term holds, if it holds one.
///
/// Brackets are also what a shell pattern writes a class of characters with, so
/// the two are told apart by what is between them: two ends and a colon are a
/// count, and everything else is the class it has always been.  A term may hold
/// both, as `web[abc][1:3]` does, and the count is the one with the colon.
fn range(term: &str) -> Option<Range<'_>> {
    let mut from = 0;

    while let Some(open) = term[from..].find('[').map(|at| from + at) {
        let close = open + term[open..].find(']')?;

        if let Some((low, high)) = term[open + 1..close].split_once(':')
            && let Some(counted) = count(low, high)
        {
            return Some(Range {
                prefix: &term[..open],
                suffix: &term[close + 1..],
                count: counted,
            });
        }

        from = open + 1;
    }

    None
}

/// The groups of hosts, and where they were read from.
pub(crate) struct Inventory {
    groups: BTreeMap<String, Vec<String>>,

    /// The file the groups came from, or the one they would have come from,
    /// which is what an error has to name to be acted on.
    path: PathBuf,
}

impl Inventory {
    /// Read the inventory, from where the command line says it is, from where
    /// the environment says it is, or from the one place it is looked for.
    ///
    /// A file that was asked for by name and is not there is a mistake and is
    /// reported.  The one that is only looked for is allowed to be absent: an
    /// inventory is a convenience, and a run that names its hosts in full works
    /// on a machine where nobody ever wrote one.
    pub(crate) fn read(named: Option<&Path>) -> Result<Self> {
        let from_env = env::var_os(ENV).map(PathBuf::from);

        if let Some(path) = named.map(Path::to_path_buf).or(from_env) {
            return Self::from_file(&path);
        }

        let path = Self::default_path();

        match path.exists() {
            true => Self::from_file(&path),
            false => Ok(Self::empty(path)),
        }
    }

    /// An inventory that names no group, which is what a run has where nobody
    /// ever wrote one.  Every host of it is then a host that was typed in full.
    pub(crate) fn empty(path: PathBuf) -> Self {
        Inventory {
            groups: BTreeMap::new(),
            path,
        }
    }

    /// Where the inventory is when nothing names another one.
    fn default_path() -> PathBuf {
        let config = match env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"),
        };

        config.join(INVENTORY)
    }

    /// Read one file, in any of the formats that a document of `detc` is
    /// written in.
    ///
    /// The `.yaml` of the name is a convention and not a rule: which format it
    /// is in is decided by what is in it, and a document that is in none of
    /// them is refused with the complaint of every parser, so the one that
    /// names a line the author recognises is there.
    fn from_file(path: &Path) -> Result<Self> {
        debug!("Reading the inventory {}", path.display());

        let document = Variables::from_file(path)
            .map_err(|e| format!("Cannot read the inventory {}: {e}", path.display()))?;

        let groups = serde_json::from_value(document.value().clone()).map_err(|e| {
            format!(
                "The inventory {} is a list of hosts under every group name: {e}",
                path.display()
            )
        })?;

        Ok(Inventory {
            groups,
            path: path.to_path_buf(),
        })
    }

    /// Every host the inventory names: the members of the groups that are
    /// neither a group nor a pattern themselves, with a range counted out into
    /// the machines it stands for.
    ///
    /// It is the set a pattern is matched against, because it is the only set
    /// of hosts there is to enumerate.  Neither DNS nor `~/.ssh/config` can be
    /// listed — a `Host web*` of that file is itself a pattern, and matching
    /// one against another says nothing about which machines exist.
    fn hosts(&self) -> BTreeSet<String> {
        let mut hosts = BTreeSet::new();

        for member in self.groups.values().flatten() {
            if self.groups.contains_key(member) {
                continue;
            }

            // A range names machines and a pattern only finds them, so what a
            // count produced belongs in the set the pattern is matched against.
            // One that counts backwards produces nothing, and is complained
            // about where it is asked for and not here
            match range(member) {
                Some(range) => hosts.extend(range.names()),
                None if !member.contains(WILDCARDS) => {
                    hosts.insert(member.clone());
                }
                None => (),
            }
        }

        hosts
    }

    /// The hosts that the command line asks for.
    ///
    /// The terms are resolved left to right, so an exclusion takes away what
    /// was already selected and never what comes after it, which is what makes
    /// `--host 'web*' --host '!web3'` read the way it looks.  A host that two
    /// terms name is reached once.
    pub(crate) fn expand(&self, terms: &[String]) -> Result<Vec<String>> {
        let mut selected: Vec<String> = Vec::new();

        for term in terms.iter().flat_map(|term| term.split(',')) {
            let term = term.trim();

            if term.is_empty() {
                continue;
            }

            let Some(excluded) = term.strip_prefix('!') else {
                for host in self.resolve(term, &mut Vec::new())? {
                    if !selected.contains(&host) {
                        selected.push(host);
                    }
                }

                continue;
            };

            if excluded.is_empty() {
                return err!(
                    "A ! takes away a host, a group or a pattern, and here it takes away nothing"
                );
            }

            let taken = self.resolve(excluded, &mut Vec::new())?;
            selected.retain(|host| !taken.contains(host));
        }

        Ok(selected)
    }

    /// What one term names: a group and everything under it, the hosts that a
    /// pattern matches, or the host itself.
    ///
    /// `chain` is the groups that are being followed to get here, so that a
    /// group that names itself is reported as the circle it is instead of
    /// recursing until the stack ends.
    fn resolve(&self, term: &str, chain: &mut Vec<String>) -> Result<Vec<String>> {
        if let Some(members) = self.groups.get(term) {
            if chain.iter().any(|group| group == term) {
                chain.push(term.to_string());
                return err!("{} is a circle of groups", chain.join(" -> "));
            }

            chain.push(term.to_string());

            let mut hosts = Vec::new();

            for member in members {
                for host in self.resolve(member, chain)? {
                    if !hosts.contains(&host) {
                        hosts.push(host);
                    }
                }
            }

            chain.pop();

            return Ok(hosts);
        }

        // A range is counted out before anything is looked up, and each name it
        // produced is resolved in turn: what a count produced may be a group or
        // a range of its own, so `r[1:2]-n[1:2]` is every pair of the two
        if let Some(range) = range(term) {
            if !range.count.ascends() {
                return err!("{term} counts backwards, and names no machine");
            }

            let mut hosts = Vec::new();

            for name in range.names() {
                for host in self.resolve(&name, chain)? {
                    if !hosts.contains(&host) {
                        hosts.push(host);
                    }
                }
            }

            return Ok(hosts);
        }

        // A host the inventory never heard of is still a host: what knows how
        // to reach one is ssh, and never this file
        if !term.contains(WILDCARDS) {
            return Ok(vec![term.to_string()]);
        }

        let pattern =
            Pattern::new(term).map_err(|e| format!("{term} is not a shell pattern: {e}"))?;

        let matched: Vec<String> = self
            .hosts()
            .into_iter()
            .filter(|host| pattern.matches(host))
            .collect();

        // A pattern that matches nothing is a typo far more often than it is an
        // empty fleet, so it is said and not run
        match (matched.is_empty(), self.groups.is_empty()) {
            (true, true) => err!(
                "There is no inventory for {term} to match against; write the groups in {}, or name the hosts",
                self.path.display()
            ),
            (true, false) => err!("{term} matches no host of {}", self.path.display()),
            _ => Ok(matched),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inventory as a file would have described it.
    fn inventory(groups: &[(&str, &[&str])]) -> Inventory {
        Inventory {
            groups: groups
                .iter()
                .map(|(name, members)| {
                    (
                        name.to_string(),
                        members.iter().map(|m| m.to_string()).collect(),
                    )
                })
                .collect(),
            ..Inventory::empty(PathBuf::from("hosts.yaml"))
        }
    }

    /// The fleet that the tests address.
    fn fleet() -> Inventory {
        inventory(&[
            ("dmz", &["web1", "web2"]),
            ("db", &["db1"]),
            ("all", &["dmz", "db"]),
        ])
    }

    /// The hosts that the terms select, as the command line gives them.
    fn expand(inventory: &Inventory, terms: &[&str]) -> Result<Vec<String>> {
        inventory.expand(&terms.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_group_is_the_hosts_it_names() {
        assert_eq!(expand(&fleet(), &["dmz"]).unwrap(), ["web1", "web2"]);
    }

    #[test]
    fn a_group_may_name_another() {
        assert_eq!(expand(&fleet(), &["all"]).unwrap(), ["web1", "web2", "db1"]);
    }

    #[test]
    fn a_comma_says_what_a_second_term_says() {
        assert_eq!(
            expand(&fleet(), &["dmz,db"]).unwrap(),
            expand(&fleet(), &["dmz", "db"]).unwrap()
        );
    }

    #[test]
    fn a_host_that_two_terms_name_is_reached_once() {
        assert_eq!(
            expand(&fleet(), &["dmz", "web1", "all"]).unwrap(),
            ["web1", "web2", "db1"]
        );
    }

    #[test]
    fn a_pattern_matches_the_hosts_of_the_inventory() {
        assert_eq!(expand(&fleet(), &["web*"]).unwrap(), ["web1", "web2"]);
        assert_eq!(expand(&fleet(), &["*1"]).unwrap(), ["db1", "web1"]);
    }

    #[test]
    fn a_name_the_inventory_never_heard_of_is_a_host() {
        assert_eq!(expand(&fleet(), &["web9"]).unwrap(), ["web9"]);

        // Which is what makes an inventory optional, and not something that
        // every run has to have written first
        let none = inventory(&[]);
        assert_eq!(expand(&none, &["web1", "web2"]).unwrap(), ["web1", "web2"]);
    }

    #[test]
    fn an_exclusion_takes_away_what_was_already_selected() {
        assert_eq!(expand(&fleet(), &["all", "!dmz"]).unwrap(), ["db1"]);
        assert_eq!(expand(&fleet(), &["web*", "!web2"]).unwrap(), ["web1"]);

        // And never what comes after it, so the order is the one that was typed
        assert_eq!(
            expand(&fleet(), &["!dmz", "all"]).unwrap(),
            ["web1", "web2", "db1"]
        );
    }

    #[test]
    fn a_group_that_names_itself_is_refused() {
        let circle = inventory(&[("a", &["b"]), ("b", &["a"])]);
        let error = expand(&circle, &["a"]).unwrap_err().to_string();

        assert!(error.contains("a -> b -> a"), "{error}");

        let itself = inventory(&[("a", &["a"])]);
        assert!(expand(&itself, &["a"]).is_err());
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_refused() {
        let error = expand(&fleet(), &["mail*"]).unwrap_err().to_string();
        assert!(error.contains("matches no host"), "{error}");

        // With no inventory at all the reason is a different one, and says
        // where the groups would go
        let error = expand(&inventory(&[]), &["mail*"]).unwrap_err().to_string();
        assert!(error.contains("no inventory"), "{error}");
    }

    #[test]
    fn a_pattern_may_stand_in_a_group() {
        let stage = inventory(&[
            ("stage", &["stage-web1", "stage-web2"]),
            ("web", &["dmz", "stage-web*"]),
            ("dmz", &["web1"]),
        ]);

        assert_eq!(
            expand(&stage, &["web"]).unwrap(),
            ["web1", "stage-web1", "stage-web2"]
        );
    }

    #[test]
    fn a_range_counts_out_the_machines_between_its_ends() {
        let none = inventory(&[]);

        assert_eq!(
            expand(&none, &["web[1:3]"]).unwrap(),
            ["web1", "web2", "web3"]
        );

        // A zero in front of the lower end is how wide every number is written
        assert_eq!(
            expand(&none, &["web[08:11]"]).unwrap(),
            ["web08", "web09", "web10", "web11"]
        );

        // A letter counts like a number, and keeps the case it was written in
        assert_eq!(
            expand(&none, &["rack[a:c]"]).unwrap(),
            ["racka", "rackb", "rackc"]
        );
        assert_eq!(
            expand(&none, &["rack[X:Z]"]).unwrap(),
            ["rackX", "rackY", "rackZ"]
        );

        // And an address is a name like any other
        assert_eq!(
            expand(&none, &["192.168.1.[10:12]"]).unwrap(),
            ["192.168.1.10", "192.168.1.11", "192.168.1.12"]
        );
    }

    #[test]
    fn a_range_names_machines_the_inventory_never_heard_of() {
        // Which is what tells it apart from a pattern: a pattern has to have
        // something to match against, and a count carries its own ends
        let none = inventory(&[]);

        assert!(expand(&none, &["web*"]).is_err());
        assert_eq!(expand(&none, &["web[1:2]"]).unwrap(), ["web1", "web2"]);
    }

    #[test]
    fn a_range_may_stand_in_a_group() {
        let lab = inventory(&[("lab", &["lab[01:03]", "gate"])]);

        assert_eq!(
            expand(&lab, &["lab"]).unwrap(),
            ["lab01", "lab02", "lab03", "gate"]
        );
    }

    #[test]
    fn a_pattern_matches_what_a_range_named() {
        // A range names machines and a pattern only finds them, so the ones a
        // count produced are there to be found
        let lab = inventory(&[("lab", &["lab[01:12]"])]);

        assert_eq!(expand(&lab, &["lab0*"]).unwrap().len(), 9);
        assert_eq!(
            expand(&lab, &["lab1?"]).unwrap(),
            ["lab10", "lab11", "lab12"]
        );
    }

    #[test]
    fn a_range_may_be_taken_away() {
        assert_eq!(
            expand(&inventory(&[]), &["web[1:4]", "!web[2:3]"]).unwrap(),
            ["web1", "web4"]
        );
    }

    #[test]
    fn two_ranges_in_one_name_are_every_pair() {
        assert_eq!(
            expand(&inventory(&[]), &["r[1:2]-n[1:2]"]).unwrap(),
            ["r1-n1", "r1-n2", "r2-n1", "r2-n2"]
        );
    }

    #[test]
    fn a_range_that_counts_backwards_is_refused() {
        let none = inventory(&[]);

        for backwards in ["web[3:1]", "rack[c:a]"] {
            let error = expand(&none, &[backwards]).unwrap_err().to_string();
            assert!(error.contains("counts backwards"), "{error}");
        }
    }

    #[test]
    fn what_lies_between_the_brackets_says_which_one_it_is() {
        let fleet = inventory(&[("dmz", &["weba", "webb", "web1"])]);

        // Without a colon the brackets are the class of characters they have
        // always been, so a pattern written before there were ranges still
        // means what it meant
        assert_eq!(expand(&fleet, &["web[ab]"]).unwrap(), ["weba", "webb"]);

        // Two ends that are not both numbers or both letters of one case are
        // not a count either, and are left to the pattern
        assert_eq!(expand(&fleet, &["web[a:1]"]).unwrap(), ["web1", "weba"]);

        // And a count is the one with two ends that go together
        assert_eq!(expand(&fleet, &["web[1:2]"]).unwrap(), ["web1", "web2"]);
    }

    #[test]
    fn nothing_is_selected_by_nothing() {
        assert!(expand(&fleet(), &[]).unwrap().is_empty());
        assert!(expand(&fleet(), &["dmz", "!dmz"]).unwrap().is_empty());
    }
}
