use super::{NodeID, Term};
use crate::encoding::{bincode, keycode};
use crate::error::{Error, Result};
use crate::storage;

use ::log::debug;
use serde::{Deserialize, Serialize};

/// A log index.
pub type Index = u64;

/// A log entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The entry index.
    pub index: Index,
    /// The term in which the entry was added.
    pub term: Term,
    /// The state machine command. None is used to commit noops during leader election.
    pub command: Option<Vec<u8>>,
}

/// A log key, encoded using KeyCode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Key {
    /// A log entry, storing the term and command.
    Entry(Index),
    /// Stores the current term and vote (if any).
    TermVote,
    /// Stores the current commit index (if any).
    CommitIndex,
}

impl Key {
    fn decode(bytes: &[u8]) -> Result<Self> {
        keycode::deserialize(bytes)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        keycode::serialize(self)
    }
}

/// Log key prefixes, used for prefix scans.
///
/// TODO: consider handling this in Key somehow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum KeyPrefix {
    Entry,
    TermVote,
    CommitIndex,
}

impl KeyPrefix {
    fn encode(&self) -> Result<Vec<u8>> {
        keycode::serialize(self)
    }
}

/// A Raft log.
pub struct Log {
    /// The underlying storage engine. Uses a trait object instead of generics,
    /// to allow runtime selection of the engine (based on the program config)
    /// and avoid propagating the generic type parameters throughout.
    engine: Box<dyn storage::Engine>,
    /// The index of the last stored entry.
    last_index: Index,
    /// The term of the last stored entry.
    last_term: Term,
    /// The index of the last committed entry.
    commit_index: Index,
    /// The term of the last committed entry.
    commit_term: Term,
}

impl Log {
    /// Creates a new log, using the given storage engine.
    pub fn new(mut engine: impl storage::Engine + 'static) -> Result<Self> {
        let (last_index, last_term) = engine
            .scan_prefix(&KeyPrefix::Entry.encode()?)
            .last()
            .transpose()?
            .map(|(k, v)| Self::decode_entry(&k, &v))
            .transpose()?
            .map(|e| (e.index, e.term))
            .unwrap_or((0, 0));
        let (commit_index, commit_term) = engine
            .get(&Key::CommitIndex.encode()?)?
            .map(|v| bincode::deserialize(&v))
            .transpose()?
            .unwrap_or((0, 0));
        Ok(Self { engine: Box::new(engine), last_index, last_term, commit_index, commit_term })
    }

    /// Decodes an entry from a log key/value pair.
    fn decode_entry(key: &[u8], value: &[u8]) -> Result<Entry> {
        if let Key::Entry(index) = Key::decode(key)? {
            Self::decode_entry_value(index, value)
        } else {
            Err(Error::Internal(format!("Invalid key {:x?}", key)))
        }
    }

    /// Decodes an entry from a value at a given index.
    fn decode_entry_value(index: Index, value: &[u8]) -> Result<Entry> {
        let (term, command) = bincode::deserialize(value)?;
        Ok(Entry { index, term, command })
    }

    /// Returns log engine name and status.
    pub fn status(&mut self) -> Result<storage::engine::Status> {
        self.engine.status()
    }

    /// Returns the commit index and term.
    pub fn get_commit_index(&self) -> (Index, Term) {
        (self.commit_index, self.commit_term)
    }

    /// Returns the last log index and term.
    pub fn get_last_index(&self) -> (Index, Term) {
        (self.last_index, self.last_term)
    }

    /// Returns the last known term (0 if none), and cast vote (if any).
    pub fn get_term(&mut self) -> Result<(Term, Option<NodeID>)> {
        let (term, voted_for) = self
            .engine
            .get(&Key::TermVote.encode()?)?
            .map(|v| bincode::deserialize(&v))
            .transpose()?
            .unwrap_or((0, None));
        debug!("Loaded term {} and voted_for {:?} from log", term, voted_for);
        Ok((term, voted_for))
    }

    /// Sets the most recent term, and cast vote (if any).
    ///
    /// TODO: rename voted_for to vote.
    pub fn set_term(&mut self, term: Term, voted_for: Option<NodeID>) -> Result<()> {
        self.engine.set(&Key::TermVote.encode()?, bincode::serialize(&(term, voted_for))?)?;
        self.engine.flush()?;
        Ok(())
    }

    /// Appends a command to the log, returning its index. None implies a noop
    /// command, typically after Raft leader changes.
    pub fn append(&mut self, term: Term, command: Option<Vec<u8>>) -> Result<Index> {
        let index = self.last_index + 1;
        self.engine.set(&Key::Entry(index).encode()?, bincode::serialize(&(term, command))?)?;
        self.engine.flush()?;
        self.last_index = index;
        self.last_term = term;
        Ok(index)
    }

    /// Commits entries up to and including the given index. The index must
    /// exist, and must be at or after the current commit index.
    ///
    /// TODO: this should take and verify the term as well, to ensure followers
    /// don't commit an entry for a divergent log.
    pub fn commit(&mut self, index: Index) -> Result<Index> {
        if index < self.commit_index {
            return Err(Error::Internal(format!(
                "Commit index regression {} -> {}",
                self.commit_index, index
            )));
        }
        let Some(entry) = self.get(index)? else {
            return Err(Error::Internal(format!("Can't commit non-existant index {}", index)));
        };
        self.engine
            .set(&Key::CommitIndex.encode()?, bincode::serialize(&(entry.index, entry.term))?)?;
        self.engine.flush()?;
        self.commit_index = entry.index;
        self.commit_term = entry.term;
        Ok(index)
    }

    /// Fetches an entry at an index, or None if it does not exist.
    pub fn get(&mut self, index: Index) -> Result<Option<Entry>> {
        self.engine
            .get(&Key::Entry(index).encode()?)?
            .map(|v| Self::decode_entry_value(index, &v))
            .transpose()
    }

    /// Checks if the log contains an entry with the given term.
    pub fn has(&mut self, index: Index, term: Term) -> Result<bool> {
        match self.get(index)? {
            Some(entry) => Ok(entry.term == term),
            None if index == 0 && term == 0 => Ok(true),
            None => Ok(false),
        }
    }

    /// Iterates over log entries in the given index range.
    pub fn scan(
        &mut self,
        range: impl std::ops::RangeBounds<Index>,
    ) -> Result<impl Iterator<Item = Result<Entry>> + '_> {
        let from = match range.start_bound() {
            std::ops::Bound::Excluded(i) => std::ops::Bound::Excluded(Key::Entry(*i).encode()?),
            std::ops::Bound::Included(i) => std::ops::Bound::Included(Key::Entry(*i).encode()?),
            std::ops::Bound::Unbounded => std::ops::Bound::Included(Key::Entry(0).encode()?),
        };
        let to = match range.end_bound() {
            std::ops::Bound::Excluded(i) => std::ops::Bound::Excluded(Key::Entry(*i).encode()?),
            std::ops::Bound::Included(i) => std::ops::Bound::Included(Key::Entry(*i).encode()?),
            std::ops::Bound::Unbounded => {
                std::ops::Bound::Included(Key::Entry(Index::MAX).encode()?)
            }
        };
        Ok(self
            .engine
            .scan_dyn((from, to))
            .map(|r| r.and_then(|(k, v)| Self::decode_entry(&k, &v))))
    }

    /// Splices a set of entries into the log. The entries must be contiguous,
    /// and the first entry must be at most last_index+1. If an entry does not
    /// exist, append it. If an existing entry has a term mismatch, replace it
    /// and all following entries.
    ///
    /// TODO: this shouldn't truncate the log if it overlaps with an infix,
    /// since message reordering can lead to data loss of committed entries.
    pub fn splice(&mut self, entries: Vec<Entry>) -> Result<Index> {
        // TODO: don't allow empty entries.
        if entries.is_empty() {
            return Ok(self.last_index);
        }
        if entries[0].index == 0 || entries[0].index > self.last_index + 1 {
            return Err(Error::Internal("Spliced entries must begin before last index".into()));
        }
        if !entries.windows(2).all(|w| w[0].index + 1 == w[1].index) {
            return Err(Error::Internal("Spliced entries must be contiguous".into()));
        }
        let (last_index, last_term) = entries.last().map(|e| (e.index, e.term)).unwrap();

        // Skip entries that are already in the log (identified by index and term).
        let mut entries = entries.as_slice();
        let mut scan = self.scan(entries[0].index..=entries.last().unwrap().index)?;
        while let Some(e) = scan.next().transpose()? {
            if e.term != entries[0].term {
                break;
            }
            entries = &entries[1..];
        }
        drop(scan);

        if !entries.is_empty() && entries[0].index <= self.commit_index {
            return Err(Error::Internal("Spliced entries must begin after commit index".into()));
        }

        // Write any entries not already in the log.
        for e in entries {
            self.engine
                .set(&Key::Entry(e.index).encode()?, bincode::serialize(&(&e.term, &e.command))?)?;
        }

        // Remove the remaining tail of the old log, if any, and update the index.
        for index in (last_index + 1)..=self.last_index {
            self.engine.delete(&Key::Entry(index).encode()?)?;
        }
        self.engine.flush()?;
        self.last_index = last_index;
        self.last_term = last_term;
        Ok(self.last_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error, result::Result};
    use test_each_file::test_each_path;

    // Run goldenscript tests in src/raft/testscripts/log.
    test_each_path! { in "src/raft/testscripts/log" as scripts => test_goldenscript }

    fn test_goldenscript(path: &std::path::Path) {
        goldenscript::run(&mut TestRunner::new(), path).expect("goldenscript failed")
    }

    /// Runs Raft log goldenscript tests. For available commands, see run().
    struct TestRunner {
        log: Log,
    }

    impl goldenscript::Runner for TestRunner {
        fn run(&mut self, command: &goldenscript::Command) -> Result<String, Box<dyn Error>> {
            let mut output = String::new();
            match command.name.as_str() {
                // append TERM [COMMAND]
                "append" => {
                    let mut args = command.consume_args();
                    let term = args.next_pos().ok_or("term not given")?.parse()?;
                    let command = args.next_pos().map(|a| a.value.as_bytes().to_vec());
                    args.reject_rest()?;
                    let index = self.log.append(term, command)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    output.push_str(&format!("append → {}\n", Self::format_entry(&entry)));
                }

                // commit INDEX
                "commit" => {
                    let mut args = command.consume_args();
                    let index = args.next_pos().ok_or("index not given")?.parse()?;
                    args.reject_rest()?;
                    let index = self.log.commit(index)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    output.push_str(&format!("commit → {}\n", Self::format_entry(&entry)));
                }

                // get INDEX...
                "get" => {
                    let mut args = command.consume_args();
                    let indexes: Vec<Index> =
                        args.rest_pos().iter().map(|a| a.parse()).collect::<Result<_, _>>()?;
                    args.reject_rest()?;
                    for index in indexes {
                        let result = match self.log.get(index)? {
                            Some(entry) => Self::format_entry(&entry),
                            None => "None".to_string(),
                        };
                        output.push_str(&format!("{result}\n"));
                    }
                }

                // get_term
                "get_term" => {
                    command.consume_args().reject_rest()?;
                    let (term, vote) = self.log.get_term()?;
                    output.push_str(&format!("term={term} vote={vote:?}\n"));
                }

                // has INDEX@TERM...
                "has" => {
                    let mut args = command.consume_args();
                    let indexes: Vec<(Index, Term)> = args
                        .rest_pos()
                        .iter()
                        .map(|a| Self::parse_index_term(&a.value))
                        .collect::<Result<_, _>>()?;
                    args.reject_rest()?;
                    for (index, term) in indexes {
                        let has = self.log.has(index, term)?;
                        output.push_str(&format!("{has}\n"));
                    }
                }

                // raw
                "raw" => {
                    command.consume_args().reject_rest()?;
                    let range = (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
                    let mut scan = self.log.engine.scan_dyn(range);
                    while let Some((key, value)) = scan.next().transpose()? {
                        output.push_str(&format!(
                            "{:?} 0x{} = 0x{}\n",
                            Key::decode(&key)?,
                            hex::encode(&key),
                            hex::encode(&value)
                        ));
                    }
                }

                // scan [RANGE]
                "scan" => {
                    let mut args = command.consume_args();
                    let range = Self::parse_index_range(
                        args.next_pos().map_or("..", |a| a.value.as_str()),
                    )?;
                    args.reject_rest()?;
                    let mut scan = self.log.scan(range)?;
                    while let Some(entry) = scan.next().transpose()? {
                        output.push_str(&format!("{}\n", Self::format_entry(&entry)));
                    }
                    if output.is_empty() {
                        output.push_str("<empty>");
                    }
                }

                // set_term TERM [VOTE]
                "set_term" => {
                    let mut args = command.consume_args();
                    let term = args.next_pos().ok_or("term not given")?.parse()?;
                    let vote = args.next_pos().map(|a| a.parse()).transpose()?;
                    args.reject_rest()?;
                    self.log.set_term(term, vote)?;
                }

                // splice [INDEX@TERM=COMMAND...]
                "splice" => {
                    let mut args = command.consume_args();
                    let mut entries = Vec::new();
                    for arg in args.rest_key() {
                        let (index, term) = Self::parse_index_term(arg.key.as_deref().unwrap())?;
                        let command = match arg.value.as_str() {
                            "" => None,
                            value => Some(value.as_bytes().to_vec()),
                        };
                        entries.push(Entry { index, term, command });
                    }
                    args.reject_rest()?;
                    let index = self.log.splice(entries)?;
                    let entry = self.log.get(index)?.expect("entry not found");
                    output.push_str(&format!("splice → {}\n", Self::format_entry(&entry)));
                }

                // status [engine=BOOL]
                "status" => {
                    let mut args = command.consume_args();
                    let engine = args.lookup_parse("engine")?.unwrap_or(false);
                    args.reject_rest()?;
                    let (commit_index, commit_term) = self.log.get_commit_index();
                    let (last_index, last_term) = self.log.get_last_index();
                    output.push_str(&format!(
                        "last={last_index}@{last_term} commit={commit_index}@{commit_term}"
                    ));
                    if engine {
                        output.push_str(&format!(" engine={:#?}", self.log.status()?));
                    }
                    output.push('\n');
                }

                name => return Err(format!("unknown command {name}").into()),
            }
            Ok(output)
        }
    }

    impl TestRunner {
        fn new() -> Self {
            let log = Log::new(crate::storage::Memory::new()).expect("log failed");
            Self { log }
        }

        /// Formats a log entry.
        fn format_entry(entry: &Entry) -> String {
            let command = match entry.command.as_ref() {
                Some(raw) => std::str::from_utf8(raw).expect("invalid command"),
                None => "None",
            };
            format!("{}@{} {command}", entry.index, entry.term)
        }

        /// Parses an index@term pair.
        fn parse_index_term(s: &str) -> Result<(Index, Term), Box<dyn Error>> {
            let re = regex::Regex::new(r"^(\d+)@(\d+)$").expect("invalid regex");
            let groups = re.captures(s).ok_or_else(|| format!("invalid index/term {s}"))?;
            let index = groups.get(1).unwrap().as_str().parse()?;
            let term = groups.get(2).unwrap().as_str().parse()?;
            Ok((index, term))
        }

        /// Parses an index range, in Rust range syntax.
        fn parse_index_range(s: &str) -> Result<impl std::ops::RangeBounds<Index>, Box<dyn Error>> {
            let mut bound =
                (std::ops::Bound::<Index>::Unbounded, std::ops::Bound::<Index>::Unbounded);
            let re = regex::Regex::new(r"^(\d+)?\.\.(=)?(\d+)?").expect("invalid regex");
            let groups = re.captures(s).ok_or_else(|| format!("invalid range {s}"))?;
            if let Some(start) = groups.get(1) {
                bound.0 = std::ops::Bound::Included(start.as_str().parse()?);
            }
            if let Some(end) = groups.get(3) {
                let end = end.as_str().parse()?;
                if groups.get(2).is_some() {
                    bound.1 = std::ops::Bound::Included(end)
                } else {
                    bound.1 = std::ops::Bound::Excluded(end)
                }
            }
            Ok(bound)
        }
    }
}
