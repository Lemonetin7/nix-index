use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use error_chain::error_chain;
use grep::matcher::{LineMatchKind, Match, Matcher, NoError};
use grep::{self};
use memchr::{memchr, memrchr};
use regex::bytes::Regex;
use regex_syntax::ast::{
    Alternation, Assertion, AssertionKind, Ast, Concat, Group, Literal, Repetition,
};
use serde_json;
use std::fs::File;
/// Creating and searching file databases.
///
/// This module implements an abstraction for creating an index of files with meta information
/// and searching that index for paths matching a specific pattern.
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use zstd;

use crate::files::{FileTree, FileTreeEntry};
use crate::frcode;
use crate::package::StorePath;

/// The version of the database format supported by this nix-index version.
///
/// This should be updated whenever you make an incompatible change to the database format.
const FORMAT_VERSION: u64 = 1;

/// The magic for nix-index database files, used to ensure that the file we're passed is
/// actually a file generated by nix-index.
const FILE_MAGIC: &'static [u8] = b"NIXI";

/// A writer for creating a new file database.
pub struct Writer {
    /// The encoder used to compress the database. Will be set to `None` when the value
    /// is dropped.
    writer: Option<BufWriter<zstd::Encoder<'static, File>>>,
}

// We need to make sure that the encoder is `finish`ed in all cases, so we need
// a custom Drop.
impl Drop for Writer {
    fn drop(&mut self) {
        if self.writer.is_some() {
            self.finish_encoder().unwrap();
        }
    }
}

impl Writer {
    /// Creates a new database at the given path with the specified zstd compression level
    /// (currently, supported values range from 0 to 22).
    pub fn create<P: AsRef<Path>>(path: P, level: i32) -> io::Result<Writer> {
        let mut file = File::create(path)?;
        file.write_all(FILE_MAGIC)?;
        file.write_u64::<LittleEndian>(FORMAT_VERSION)?;
        let mut encoder = zstd::Encoder::new(file, level)?;
        encoder.multithread(num_cpus::get() as u32)?;

        Ok(Writer {
            writer: Some(BufWriter::new(encoder)),
        })
    }

    /// Add a new package to the database for the given store path with its corresponding
    /// file tree. Entries are only added if they match `filter_prefix`.
    pub fn add(
        &mut self,
        path: StorePath,
        files: FileTree,
        filter_prefix: &[u8],
    ) -> io::Result<()> {
        let writer = self.writer.as_mut().expect("not dropped yet");
        let mut encoder =
            frcode::Encoder::new(writer, b"p".to_vec(), serde_json::to_vec(&path).unwrap());
        for entry in files.to_list(filter_prefix) {
            entry.encode(&mut encoder)?;
        }
        Ok(())
    }

    /// Finishes encoding. After calling this function, `add` may no longer be called, since this function
    /// closes the stream.
    ///
    /// The return value is the underlying File.
    fn finish_encoder(&mut self) -> io::Result<File> {
        let writer = self.writer.take().expect("not dropped yet");
        let encoder = writer.into_inner()?;
        encoder.finish()
    }

    /// Finish the encoding and return the size in bytes of the compressed file that was created.
    pub fn finish(mut self) -> io::Result<u64> {
        let mut file = self.finish_encoder()?;
        file.seek(SeekFrom::Current(0))
    }
}

error_chain! {
    errors {
        UnsupportedFileType(found: Vec<u8>) {
            description("unsupported file type")
            display("expected file to start with nix-index file magic 'NIXI', but found '{}' (is this a valid nix-index database file?)", String::from_utf8_lossy(found))
        }
        UnsupportedVersion(found: u64) {
            description("unsupported file version")
            display("this executable only supports the nix-index database version {}, but found a database with version {}", FORMAT_VERSION, found)
        }
        MissingPackageEntry {
            description("missing package entry for path")
            display("database corrupt, found a file entry without a matching package entry")
        }
        Frcode(err: frcode::Error) {
            description("frcode error")
            display("database corrupt, frcode error: {}", err)
        }
        EntryParse(entry: Vec<u8>) {
            description("entry parse failure")
            display("database corrupt, could not parse entry: {:?}", String::from_utf8_lossy(entry))
        }
        StorePathParse(path: Vec<u8>) {
            description("store path parse failure")
            display("database corrupt, could not parse store path: {:?}", String::from_utf8_lossy(path))
        }
    }

    foreign_links {
        Io(io::Error);
        Grep(grep::regex::Error);
    }
}

impl From<frcode::Error> for Error {
    fn from(err: frcode::Error) -> Error {
        ErrorKind::Frcode(err).into()
    }
}

/// A Reader allows fast querying of a nix-index database.
pub struct Reader {
    decoder: frcode::Decoder<BufReader<zstd::Decoder<'static, BufReader<File>>>>,
}

impl Reader {
    /// Opens a nix-index database located at the given path.
    ///
    /// If the path does not exist or is not a valid database, an error is returned.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Reader> {
        let mut file = File::open(path)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;

        if magic != FILE_MAGIC {
            return Err(ErrorKind::UnsupportedFileType(magic.to_vec()).into());
        }

        let version = file.read_u64::<LittleEndian>()?;
        if version != FORMAT_VERSION {
            return Err(ErrorKind::UnsupportedVersion(version).into());
        }

        let decoder = zstd::Decoder::new(file)?;
        Ok(Reader {
            decoder: frcode::Decoder::new(BufReader::new(decoder)),
        })
    }

    /// Builds a query to find all entries in the database that have a filename matching the given pattern.
    ///
    /// Afterwards, use `Query::into_iter` to iterate over the items.
    pub fn query(self, exact_regex: &Regex) -> Query {
        Query {
            reader: self,
            exact_regex: exact_regex,
            hash: None,
            package_pattern: None,
        }
    }

    /// Dumps the contents of the database to stdout, for debugging.
    #[allow(clippy::print_stdout)]
    pub fn dump(&mut self) -> Result<()> {
        loop {
            let block = self.decoder.decode()?;
            if block.is_empty() {
                break;
            }
            for line in block.split(|c| *c == b'\n') {
                println!("{:?}", String::from_utf8_lossy(line));
            }
            println!("-- block boundary");
        }
        Ok(())
    }
}

/// A builder for a `ReaderIter` to iterate over entries in the database matching a given pattern.
pub struct Query<'a, 'b> {
    /// The underlying reader from which we read input.
    reader: Reader,

    /// The pattern that file paths have to match.
    exact_regex: &'a Regex,

    /// Only include the package with the given hash.
    hash: Option<String>,

    /// Only include packages whose name matches the given pattern.
    package_pattern: Option<&'b Regex>,
}

impl<'a, 'b> Query<'a, 'b> {
    /// Limit results to entries from the package with the specified hash if `Some`.
    pub fn hash(self, hash: Option<String>) -> Query<'a, 'b> {
        Query { hash: hash, ..self }
    }

    /// Limit results to entries from packages whose name matches the given regex if `Some`.
    pub fn package_pattern(self, package_pattern: Option<&'b Regex>) -> Query<'a, 'b> {
        Query {
            package_pattern: package_pattern,
            ..self
        }
    }

    /// Runs the query, returning an Iterator that will yield all entries matching the conditions.
    ///
    /// There is no guarantee about the order of the returned matches.
    pub fn run(self) -> Result<ReaderIter<'a, 'b>> {
        let mut expr = regex_syntax::ast::parse::Parser::new()
            .parse(self.exact_regex.as_str())
            .expect("regex cannot be invalid");
        // replace the ^ anchor by a NUL byte, since each entry is of the form `METADATA\0PATH`
        // (so the NUL byte marks the start of the path).
        {
            let mut stack = vec![&mut expr];
            while let Some(e) = stack.pop() {
                match *e {
                    Ast::Assertion(Assertion {
                        kind: AssertionKind::StartLine,
                        span,
                    }) => {
                        *e = Ast::Literal(Literal {
                            span,
                            c: '\0',
                            kind: regex_syntax::ast::LiteralKind::Verbatim,
                        })
                    }
                    Ast::Group(Group { ref mut ast, .. }) => stack.push(ast),
                    Ast::Repetition(Repetition { ref mut ast, .. }) => stack.push(ast),
                    Ast::Concat(Concat { ref mut asts, .. })
                    | Ast::Alternation(Alternation { ref mut asts, .. }) => stack.extend(asts),
                    _ => {}
                }
            }
        }
        let mut regex_builder = grep::regex::RegexMatcherBuilder::new();
        regex_builder.line_terminator(Some(b'\n')).multi_line(true);

        let grep = regex_builder.build(&format!("{}", expr))?;
        Ok(ReaderIter {
            reader: self.reader,
            found: Vec::new(),
            found_without_package: Vec::new(),
            pattern: grep,
            exact_pattern: self.exact_regex,
            package_entry_pattern: regex_builder.build("^p\0").expect("valid regex"),
            package_name_pattern: self.package_pattern,
            package_hash: self.hash,
        })
    }
}

/// An iterator for entries in a database matching a given pattern.
pub struct ReaderIter<'a, 'b> {
    /// The underlying reader from which we read input.
    reader: Reader,
    /// Entries that matched the pattern but have not been returned by `next` yet.
    found: Vec<(StorePath, FileTreeEntry)>,
    /// Entries that matched the pattern but for which we don't know yet what package they belong to.
    /// This may happen if the entry we matched was at the end of the search buffer, so that the entry
    /// for the package did not fit into the buffer anymore (since the package is stored after the entries
    /// of the package). In this case, we need to look for the package entry in the next iteration when
    /// we read the next block of input.
    found_without_package: Vec<FileTreeEntry>,
    /// The pattern for which to search package paths.
    ///
    /// This pattern should work on the raw bytes of file entries. In particular, the file path is not the
    /// first data in a file entry, so the regex `^` anchor will not work correctly.
    ///
    /// The pattern here may produce false positives (for example, if it matches inside the metadata of a file
    /// entry). This is not a problem, as matches are later checked against `exact_pattern`.
    pattern: grep::regex::RegexMatcher,
    /// The raw pattern, as supplied to `find_iter`. This is used to verify matches, since `pattern` itself
    /// may produce false positives.
    exact_pattern: &'a Regex,
    /// Pattern that matches only package entries.
    package_entry_pattern: grep::regex::RegexMatcher,
    /// Pattern that the package name should match.
    package_name_pattern: Option<&'b Regex>,
    /// Only search the package with the given hash.
    package_hash: Option<String>,
}

fn consume_no_error<T>(e: NoError) -> T {
    panic!("impossible: {}", e)
}

fn next_matching_line<M: Matcher<Error = NoError>>(
    matcher: M,
    buf: &[u8],
    mut start: usize,
) -> Option<Match> {
    while let Some(candidate) = matcher
        .find_candidate_line(&buf[start..])
        .unwrap_or_else(consume_no_error)
    {
        let (pos, confirmed) = match candidate {
            LineMatchKind::Confirmed(pos) => (start + pos, true),
            LineMatchKind::Candidate(pos) => (start + pos, false),
        };

        let line_start = memrchr(b'\n', &buf[..pos]).map(|x| x + 1).unwrap_or(0);
        let line_end = memchr(b'\n', &buf[pos..])
            .map(|x| x + pos + 1)
            .unwrap_or(buf.len());

        if !confirmed
            && !matcher
                .is_match(&buf[line_start..line_end])
                .unwrap_or_else(consume_no_error)
        {
            start = line_end;
            continue;
        }

        return Some(Match::new(line_start, line_end));
    }
    None
}

impl<'a, 'b> ReaderIter<'a, 'b> {
    /// Reads input until `self.found` contains at least one entry or the end of the input has been reached.
    #[allow(unused_assignments)] // because of https://github.com/rust-lang/rust/issues/22630
    fn fill_buf(&mut self) -> Result<()> {
        // the input is processed in blocks until we've found at least a single entry
        while self.found.is_empty() {
            let &mut ReaderIter {
                ref mut reader,
                ref package_entry_pattern,
                ref package_name_pattern,
                ref package_hash,
                ..
            } = self;
            let block = reader.decoder.decode()?;

            // if the block is empty, the end of input has been reached
            if block.is_empty() {
                return Ok(());
            }

            // when we find a match, we need to know the package that this match belongs to.
            // the `find_package` function will skip forward until a package entry is found
            // (the package entry comes after all file entries for a package).
            //
            // to be more efficient if there are many matches, we cache the current package here.
            // this package is valid for all positions up to the second element of the tuple
            // (after that, a new package begins).
            let mut cached_package: Option<(StorePath, usize)> = None;
            let mut no_more_package = false;
            let mut find_package = |item_end| -> Result<_> {
                if let Some((ref pkg, end)) = cached_package {
                    if item_end < end {
                        return Ok(Some((pkg.clone(), end)));
                    }
                }

                if no_more_package {
                    return Ok(None);
                }

                let mat = match next_matching_line(&package_entry_pattern, &block, item_end) {
                    Some(v) => v,
                    None => {
                        no_more_package = true;
                        return Ok(None);
                    }
                };

                let json = &block[mat.start() + 2..mat.end() - 1];
                let pkg: StorePath = serde_json::from_slice(json)
                    .chain_err(|| ErrorKind::StorePathParse(json.to_vec()))?;
                cached_package = Some((pkg.clone(), mat.end()));
                Ok(Some((pkg, mat.end())))
            };

            // Tests if a store path matches the `package_name_pattern` and `package_hash` constraints.
            let should_search_package = |pkg: &StorePath| -> bool {
                package_name_pattern.map_or(true, |r| r.is_match(pkg.name().as_bytes()))
                    && package_hash.as_ref().map_or(true, |h| h == &pkg.hash())
            };

            let mut pos = 0;
            // if there are any entries without a package left over from the previous iteration, see
            // if this block contains the package entry.
            if !self.found_without_package.is_empty() {
                if let Some((pkg, end)) = find_package(0)? {
                    if !should_search_package(&pkg) {
                        // all entries before end will have the same package
                        pos = end;
                        self.found_without_package.truncate(0);
                    } else {
                        for entry in self.found_without_package.split_off(0) {
                            self.found.push((pkg.clone(), entry));
                        }
                    }
                }
            }

            // process all matches in this block
            while let Some(mat) = next_matching_line(&self.pattern, &block, pos) {
                pos = mat.end();
                let entry = &block[mat.start()..mat.end() - 1];
                // skip entries that aren't describing file paths
                if self
                    .package_entry_pattern
                    .is_match(entry)
                    .unwrap_or_else(consume_no_error)
                {
                    continue;
                }

                // skip if package name or hash doesn't match
                // we can only skip if we know the package
                if let Some((pkg, end)) = find_package(mat.end())? {
                    if !should_search_package(&pkg) {
                        // all entries before end will have the same package
                        pos = end;
                        continue;
                    }
                }

                let entry = FileTreeEntry::decode(entry)
                    .ok_or_else(|| Error::from(ErrorKind::EntryParse(entry.to_vec())))?;

                // check for false positives
                if !self.exact_pattern.is_match(&entry.path) {
                    continue;
                }

                match find_package(mat.end())? {
                    None => self.found_without_package.push(entry),
                    Some((pkg, _)) => self.found.push((pkg, entry)),
                }
            }
        }
        Ok(())
    }

    /// Returns the next match in the database.
    fn next_match(&mut self) -> Result<Option<(StorePath, FileTreeEntry)>> {
        self.fill_buf()?;
        Ok(self.found.pop())
    }
}

impl<'a, 'b> Iterator for ReaderIter<'a, 'b> {
    type Item = Result<(StorePath, FileTreeEntry)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_match() {
            Err(e) => Some(Err(e)),
            Ok(v) => v.map(Ok),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_matching_line_package() {
        let matcher = grep::regex::RegexMatcherBuilder::new()
            .line_terminator(Some(b'\n'))
            .multi_line(true)
            .build("^p")
            .expect("valid regex");
        let buffer = br#"
SOME LINE
pDATA
ANOTHER LINE
        "#;

        let mat = next_matching_line(matcher, buffer, 0);
        assert_eq!(mat, Some(Match::new(11, 17)));
    }
}
