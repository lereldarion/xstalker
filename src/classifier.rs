use super::{ActiveWindowMetadata, ErrorMessage, UniqueCategories};
use std;
use std::io;
use std::io::{BufRead, BufReader};
use std::process;

/// Classifier: determines the category based on active window metadata.
pub trait Classifier {
    /// Returns the set of all categories defined in the classifier.
    fn categories(&self) -> Result<UniqueCategories, ErrorMessage>;

    /// Returns the category name for the metadata, or None if not matched.
    /// The category must be in the set returned by categories().
    fn classify(&self, metadata: &ActiveWindowMetadata) -> Result<Option<String>, ErrorMessage>;
}

/** Classify using an external process.
 *
 */
pub struct ExternalProcess {
    child: process::Child,
    stdin: process::ChildStdin,
    stdout: BufReader<process::ChildStdout>,
}

impl ExternalProcess {
    pub fn new(program: &str) -> Result<Self, ErrorMessage> {
        let mut child = process::Command::new(program)
            .stdin(process::Stdio::piped())
            .stdout(process::Stdio::piped())
            .spawn()
            .map_err(|e| ErrorMessage::new(format!("Cannot spawn subprocess '{}'", program), e))?;
        // Extract piped IO descriptors
        let stdin =
            std::mem::replace(&mut child.stdin, None).expect("Child process must have stdin");
        let stdout =
            std::mem::replace(&mut child.stdout, None).expect("Child process must have stdout");
        Ok(ExternalProcess {
            child: child,
            stdin: stdin,
            stdout: BufReader::new(stdout),
        })
    }
}
impl Drop for ExternalProcess {
    fn drop(&mut self) {
        // FIXME do something with return code ?
        self.child.wait().expect("Child process wait() failed");
    }
}
impl Classifier for ExternalProcess {
    fn categories(&self) -> Result<UniqueCategories, ErrorMessage> {
        Ok(UniqueCategories(Vec::new()))
    }
    fn classify(&self, metadata: &ActiveWindowMetadata) -> Result<Option<String>, ErrorMessage> {
        Ok(None)
    }
}

/** TestClassifier: stores rules used to determine categories for time spent.
 * Rules are stored in an ordered list.
 * The first matching rule in the list chooses the category.
 * A category can appear in multiple rules.
 */
pub struct TestClassifier {
    filters: Vec<(String, Box<Fn(&ActiveWindowMetadata) -> bool>)>,
}
impl TestClassifier {
    /// Create a new classifier with no rules.
    pub fn new() -> Self {
        let mut classifier = TestClassifier {
            filters: Vec::new(),
        };
        classifier.append_filter(&"coding", |md| {
            md.class
                .as_ref()
                .map(|class| class == "konsole")
                .unwrap_or(false)
        });
        classifier.append_filter(&"unknown", |_| true);
        classifier
    }
    /// Add a rule at the end of the list, for the given category.
    fn append_filter<F>(&mut self, category: &str, filter: F)
    where
        F: 'static + Fn(&ActiveWindowMetadata) -> bool,
    {
        self.filters
            .push((String::from(category), Box::new(filter)));
    }
}
impl Classifier for TestClassifier {
    fn categories(&self) -> Result<UniqueCategories, ErrorMessage> {
        Ok(UniqueCategories::make_unique(
            self.filters
                .iter()
                .map(|(category, _)| category.clone())
                .collect(),
        ))
    }

    fn classify(&self, metadata: &ActiveWindowMetadata) -> Result<Option<String>, ErrorMessage> {
        Ok(self.filters
            .iter()
            .find(|(_category, filter)| filter(metadata))
            .map(|(category, _filter)| category.clone()))
    }
}