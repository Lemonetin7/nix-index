use xml;
use std::io::{self, Read};
use xml::reader::{EventReader, XmlEvent};
use xml::common::{TextPosition, Position};
use std::process::{Command, Stdio, Child, ChildStdout};
use std::fmt;

use package::{PathOrigin, StorePath};

pub struct PackagesParser<R: Read> {
    events: EventReader<R>,
    current_item: Option<String>,
}

#[derive(Debug)]
pub struct ParserError {
    position: TextPosition,
    kind: ParserErrorKind,
}

#[derive(Debug)]
enum ParserErrorKind {
    MissingParent {
        element_name: String,
        expected_parent: String,
    },
    ParentNotAllowed {
        element_name: String,
        found_parent: String,
    },
    MissingAttribute {
        element_name: String,
        attribute_name: String,
    },
    MissingStartTag {
        element_name: String,
    },
    XmlError {
        error: xml::reader::Error,
    },
    InvalidStorePath {
        path: String,
    },
}

impl fmt::Display for ParserError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use self::ParserErrorKind::*;
        write!(f, "error at {}: ", self.position)?;
        match self.kind {
            MissingParent { ref element_name, ref expected_parent  } =>
                write!(f, "element {} appears outside of expected parent {}", element_name, expected_parent),
            ParentNotAllowed { ref element_name, ref found_parent } =>
                write!(f, "element {} must not appear as child of {}", element_name, found_parent),
            MissingAttribute { ref element_name, ref attribute_name } =>
                write!(f, "element {} must have an attribute named {}", element_name, attribute_name),
            MissingStartTag { ref element_name } =>
                write!(f, "element {} does not have a start tag", element_name),
            XmlError { ref error } =>
                write!(f, "document not well-formed: {}", error),
            InvalidStorePath { ref path } =>
                write!(f, "store path does not match expected format /prefix/hash-name: {}", path)
        }
    }
}

impl<R: Read> PackagesParser<R> {
    pub fn new(reader: R) -> PackagesParser<R> {
        PackagesParser { events: EventReader::new(reader), current_item: None }
    }

    fn err(&self, kind: ParserErrorKind) -> ParserError {
        ParserError { position: self.events.position(), kind: kind }
    }

    fn next_err(&mut self) -> Result<Option<StorePath>, ParserError> {
        use self::XmlEvent::*;
        use self::ParserErrorKind::*;

        loop {
            let event = self.events.next().map_err(|e| self.err(XmlError { error: e}))?;
            match event {
                StartElement { name: element_name, attributes, .. } => {
                    if element_name.local_name == "item" {
                        if !self.current_item.is_none() {
                            return Err(self.err(ParentNotAllowed {
                                element_name: "item".to_string(),
                                found_parent: "item".to_string(),
                            }))
                        }

                        let attr_path = attributes.into_iter().find(|a| a.name.local_name == "attrPath");
                        let attr_path = attr_path.ok_or( self.err(MissingAttribute {
                            element_name: "item".into(),
                            attribute_name: "attrPath".into(),
                        }) )?;

                        self.current_item = Some(attr_path.value);
                        continue
                    }

                    if element_name.local_name == "output" {
                        if let Some(item) = self.current_item.clone() {
                            let mut output_name = None;
                            let mut output_path = None;

                            for attr in attributes {
                                if attr.name.local_name == "name" {
                                    output_name = Some(attr.value);
                                    continue
                                }

                                if attr.name.local_name == "path" {
                                    output_path = Some(attr.value);
                                    continue
                                }
                            }

                            let output_name = output_name.ok_or( self.err(MissingAttribute {
                                element_name: "output".into(),
                                attribute_name: "name".into(),
                            }) )?;

                            let output_path = output_path.ok_or( self.err(MissingAttribute {
                                element_name: "output".into(),
                                attribute_name: "path".into(),
                            }) )?;

                            let origin = PathOrigin { attr: item, output: output_name, toplevel: true };
                            let store_path = StorePath::parse(origin, &output_path);
                            let store_path = store_path.ok_or( self.err(InvalidStorePath { path: output_path }) )?;

                            return Ok(Some(store_path))
                        } else {
                            return Err(self.err(MissingParent {
                                element_name: "output".into(),
                                expected_parent: "item".into(),
                            })
)                        }
                    }
                },

                EndElement { name: element_name } => {
                    if element_name.local_name == "item" {
                        if self.current_item.is_none() {
                            return Err(self.err(MissingStartTag { element_name: "item".into()}));
                        }
                        self.current_item = None
                    }
                },

                EndDocument => { break }

                _ => {}
            }
        }

        Ok(None)
    }

}

impl<R: Read> Iterator for PackagesParser<R> {
    type Item = Result<StorePath, ParserError>;

    fn next(&mut self) -> Option<Result<StorePath, ParserError>> {
        match self.next_err() {
            Err(e) => Some(Err(e)),
            Ok(Some(i)) => Some(Ok(i)),
            Ok(None) => None,
        }

    }

}

#[derive(Debug)]
pub enum Error {
    Parse(ParserError),
    Io(io::Error),
    Command(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use self::Error::*;
        match *self {
            Parse(ref e) => write!(f, "parsing XML output of nix-env failed: {}", e),
            Io(ref e) => write!(f, "IO error: {}", e),
            Command(ref e) => write!(f, "nix-env failed with error: {}", e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error { Error::Io(err) }
}


impl From<ParserError> for Error {
    fn from(err: ParserError) -> Error { Error::Parse(err) }
}

pub struct PackagesQuery<R: Read> {
    parser: Option<PackagesParser<R>>,
    child: Child,
}

impl<R: Read> PackagesQuery<R> {
    fn check_error(&mut self) -> Option<Error> {
        (|| {
            let status = self.child.wait()?;

            if !status.success() {
                let mut message = String::new();
                self.child.stderr.take().expect("should have stderr pipe").read_to_string(&mut message)?;

                return Err(Error::Command(match status.code() {
                    Some(c) => format!("nix-env failed with exit code {}:\n{}", c, message),
                    None    => format!("nix-env failed with unknown exit code:\n{}", message),
                }))
            }

            Ok(())
        })().err()
    }
}

impl<R: Read> Iterator for PackagesQuery<R> {
    type Item = Result<StorePath, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        self.parser.take().and_then(|mut parser| {
            parser.next().map(|v| {
                self.parser = Some(parser);
                v.map_err(|e| self.check_error().unwrap_or(Error::from(e)))
            }).or_else(|| {
                self.parser = None;
                self.check_error().map(Err)
            })
        })
    }
}


pub fn query_packages(nixpkgs: &str) -> Result<PackagesQuery<ChildStdout>, Error> {
    let mut child = Command::new("nix-env")
        .arg("-qaP")
        .arg("--out-path")
        .arg("--xml")
        .arg("--arg").arg("config").arg("{}")
        .arg("--file").arg(nixpkgs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("should have stdout pipe");
    let packages = PackagesParser::new(stdout);

    Ok(PackagesQuery { parser: Some(packages), child: child })
}