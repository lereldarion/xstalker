#![deny(deprecated)]
extern crate chrono;
#[macro_use]
extern crate clap;
extern crate tokio;
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;
use std::time;
use tokio::prelude::*;

/// Generic error type: contains a message and a boxed inner error if applicable.
#[derive(Debug)]
pub struct ErrorMessage {
    message: String,
    inner: Option<Box<dyn Error + Send + Sync>>,
}
impl ErrorMessage {
    pub fn new<M, E>(message: M, cause: E) -> Self
    where
        M: Into<String>,
        E: Error + Send + Sync + 'static,
    {
        ErrorMessage {
            message: message.into(),
            inner: Some(Box::new(cause)),
        }
    }
}
impl<T: Into<String>> From<T> for ErrorMessage {
    fn from(t: T) -> Self {
        ErrorMessage {
            message: t.into(),
            inner: None,
        }
    }
}
impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl Error for ErrorMessage {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.inner {
            Some(b) => Some(b.as_ref()),
            None => None,
        }
    }
}

/// Store a set of unique category names, in a specific order.
#[derive(Debug, Clone)]
pub struct UniqueCategories(Vec<String>);
impl UniqueCategories {
    /// Check if given vec has unique elements
    pub fn from_unique(categories: Vec<String>) -> Result<Self, ErrorMessage> {
        for category in &categories {
            if categories.iter().filter(|c| *c == category).count() > 1 {
                return Err(ErrorMessage::from(format!(
                    "Duplicate category '{}'",
                    category
                )));
            }
        }
        Ok(UniqueCategories(categories))
    }
    /// Make given vec unique. Order is not conserved.
    pub fn make_unique(mut categories: Vec<String>) -> Self {
        categories.sort();
        categories.dedup();
        UniqueCategories(categories)
    }
    /// Extend current vec with new categories only. Return slice to inserted elements.
    pub fn extend(&mut self, categories: UniqueCategories) -> usize {
        let v = &mut self.0;
        let initial_len = v.len();
        for c in categories.0 {
            if !v[..initial_len].contains(&c) {
                v.push(c);
            }
        }
        v.len() - initial_len
    }
}
impl std::ops::Deref for UniqueCategories {
    type Target = [String];
    fn deref(&self) -> &[String] {
        &self.0
    }
}

/// Metadata for the current active window
#[derive(Debug)]
pub struct ActiveWindowMetadata {
    title: Option<String>,
    class: Option<String>,
}

/// Classifier trait and impls.
mod classifier;
use classifier::Classifier;

/// Database time recording
mod database;
use database::{CategoryDurationCounter, Database, DatabaseTime};

/// Xcb interface
mod xcb_stalker;
use xcb_stalker::ActiveWindowChanges;

fn write_durations_to_disk(
    db: &mut Database,
    duration_counter: &mut CategoryDurationCounter,
    window_start: &DatabaseTime,
    timestamp: time::Instant,
) -> io::Result<()> {
    duration_counter.record_current_duration(timestamp);
    db.rewrite_last_entry(window_start, duration_counter.durations())
}

fn change_time_window(
    db: &mut Database,
    duration_counter: &mut CategoryDurationCounter,
    window_start: &mut DatabaseTime,
    time_window_size: time::Duration,
    timestamp: time::Instant,
) -> io::Result<()> {
    // Flush current durations values
    write_durations_to_disk(db, duration_counter, window_start, timestamp)?;
    // Create a new time window
    db.lock_last_entry();
    duration_counter.reset_durations();
    *window_start = *window_start + chrono::Duration::from_std(time_window_size).unwrap();
    Ok(())
}

fn run_daemon(
    classifier: &mut dyn Classifier,
    db_file: &Path,
    db_write_interval: time::Duration,
    time_window_size: time::Duration,
) -> Result<(), ErrorMessage> {
    let db_filename = db_file.display();
    // Setup state
    let classifier_categories = classifier.categories();
    let mut db = Database::open(db_file, classifier_categories)
        .map_err(|e| ErrorMessage::new(format!("Unable to open database '{}'", db_filename), e))?;
    let mut duration_counter = CategoryDurationCounter::new(db.categories().clone());
    let active_window_changes = ActiveWindowChanges::new()
        .map_err(|e| ErrorMessage::new("Unable to start window event listener", e))?;

    // Determine current time window
    let now = DatabaseTime::from(time::SystemTime::now());
    let window_start = {
        if let Some((time, durations)) = db.get_last_entry().map_err(|e| {
            ErrorMessage::new(format!("Unable to read last entry of '{}'", db_filename), e)
        })? {
            if time <= now && now < time + chrono::Duration::from_std(time_window_size).unwrap() {
                // We are still in the time window of the last entry, resume the window.
                duration_counter.set_durations(durations);
                time
            } else {
                // Outside of last entry time window: create a new window.
                // This includes the case where now < time (timezone change, system clock adjustement).
                db.lock_last_entry();
                now
            }
        } else {
            // No last entry: create new window.
            now
        }
    };
    let duration_to_next_window_change = time_window_size
        - chrono::Duration::to_std(&now.signed_duration_since(window_start)).unwrap();

    // Set initial category
    {
        let (initial_metadata, timestamp) = active_window_changes
            .get_current_metadata()
            .map_err(|e| ErrorMessage::new("Unable to get window metadata", e))?;
        let initial_category = classifier.classify(initial_metadata)?;
        duration_counter.category_changed(initial_category, timestamp);
    }

    // Wrap shared state in RefCell: cannot prove with type that mutations are exclusive.
    let db = RefCell::new(db);
    let duration_counter = RefCell::new(duration_counter);
    let window_start = RefCell::new(window_start);

    // Listen to active window changes.
    let all_category_changes = active_window_changes
        .map_err(|e| ErrorMessage::new("Window metadata listener failed", e))
        .for_each(|(active_window_metadata, timestamp)| {
            println!("task_handle_window_change");
            let category = classifier.classify(active_window_metadata)?;
            duration_counter
                .borrow_mut()
                .category_changed(category, timestamp);
            Ok(())
        });

    // Periodically write database to file
    let all_db_writes =
        tokio::timer::Interval::new(time::Instant::now() + db_write_interval, db_write_interval)
            .map_err(|e| ErrorMessage::new("Timer error", e))
            .for_each(|instant| {
                println!("task_write_db");
                write_durations_to_disk(
                    &mut db.borrow_mut(),
                    &mut duration_counter.borrow_mut(),
                    &window_start.borrow(),
                    instant,
                )
                .map_err(|e| {
                    ErrorMessage::new(format!("Unable to write to database '{}'", db_filename), e)
                })
            });

    // Periodically change time window
    let all_time_window_changes = tokio::timer::Interval::new(
        time::Instant::now() + duration_to_next_window_change,
        time_window_size,
    )
    .map_err(|e| ErrorMessage::new("Timer error", e))
    .for_each(|instant| {
        println!("task_new_time_window");
        change_time_window(
            &mut db.borrow_mut(),
            &mut duration_counter.borrow_mut(),
            &mut window_start.borrow_mut(),
            time_window_size,
            instant,
        )
        .map_err(|e| ErrorMessage::new(format!("Unable to write to database '{}'", db_filename), e))
    });

    // Create a tokio runtime to implement an event loop.
    // Single threaded is enough.
    // TODO support signals using tokio_signal crate ?
    let mut runtime = tokio::runtime::current_thread::Runtime::new()
        .map_err(|e| ErrorMessage::new("Unable to create tokio runtime", e))?;
    runtime.block_on(
        Future::join3(all_category_changes, all_db_writes, all_time_window_changes)
            .map(|(_, _, _)| ()),
    )
}

fn do_main() -> Result<(), ErrorMessage> {
    let matches = app_from_crate!()
        .setting(clap::AppSettings::VersionlessSubcommands)
        .setting(clap::AppSettings::SubcommandRequired)
        .arg(
            clap::Arg::with_name("db_file")
                .help("Path to database file used to store activity")
                .required(true)
                .index(1),
        )
        .arg(
            clap::Arg::with_name("time-window")
                .long("time-window")
                .help("Maximum time window covered by a database entry")
                .takes_value(true)
                .value_name("time_secs")
                .default_value("3600"),
        )
        .arg(
            clap::Arg::with_name("db-write")
                .long("db-write")
                .help("Interval at which the database is written to disk")
                .takes_value(true)
                .value_name("time_secs")
                .default_value("60"),
        )
        .subcommand(
            clap::SubCommand::with_name("process")
                .about("Classify by using an external subprocess")
                .after_help(classifier::Process::doc())
                .setting(clap::AppSettings::TrailingVarArg)
                .arg(
                    clap::Arg::with_name("command")
                        .help("Subprocess command")
                        .required(true)
                        .index(1),
                )
                .arg(
                    clap::Arg::with_name("args")
                        .help("Subprocess arguments")
                        .index(2)
                        .multiple(true),
                ),
        )
        .get_matches();

    let time_window_size_secs = matches
        .value_of("time-window")
        .unwrap()
        .parse()
        .map_err(|e| ErrorMessage::new("Unable to parse time window", e))?;
    let db_write_interval_secs = matches
        .value_of("db-write")
        .unwrap()
        .parse()
        .map_err(|e| ErrorMessage::new("Unable to parse time window", e))?;
    if !(0 < db_write_interval_secs && db_write_interval_secs < time_window_size_secs) {
        return Err(ErrorMessage::from(
            "Wrong time intervals: must follow 0 < db_write < time_window",
        ));
    }

    let mut process_classifier;
    let classifier: &mut dyn Classifier = match matches.subcommand() {
        ("process", Some(process_args)) => {
            let command_name = process_args.value_of_os("command").unwrap();
            let command_args = process_args.values_of_os("args").unwrap_or_default();
            process_classifier = classifier::Process::new(command_name, command_args)
                .map_err(|e| ErrorMessage::new("Cannot create subprocess classifier", e))?;
            &mut process_classifier
        }
        _ => panic!("Argument parsing: subcommand is mandatory"),
    };

    run_daemon(
        classifier,
        Path::new(matches.value_of_os("db_file").unwrap()),
        time::Duration::from_secs(db_write_interval_secs),
        time::Duration::from_secs(time_window_size_secs),
    )
}

/** If main returns Result<_, E>, E will be printed with fmt::Debug.
 * Wrap an Error in this to print a newline delimited error message.
 */
fn main() -> Result<(), ShowErrorTraceback<ErrorMessage>> {
    do_main().map_err(|e| ShowErrorTraceback(e))
}

/// Print error causes in a traceback fashion
struct ShowErrorTraceback<T: Error>(T);
impl<T: Error> fmt::Debug for ShowErrorTraceback<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.0)?;
        for err in Traceback(self.0.source()) {
            write!(f, ":\n{}", err)?;
        }
        Ok(())
    }
}

/// Iterate on error causes
struct Traceback<'a>(Option<&'a dyn Error>);
impl<'a> Iterator for Traceback<'a> {
    type Item = &'a dyn Error;
    fn next(&mut self) -> Option<Self::Item> {
        let current = self.0;
        self.0 = match &current {
            Some(err) => err.source(),
            None => None,
        };
        current
    }
}
