#![deny(deprecated)]
extern crate chrono;
extern crate tokio;
use std::cell::RefCell;
use std::fmt;
use std::io;
use std::path::Path;
use std::time;
use tokio::prelude::*;

#[derive(Debug)]
pub struct ActiveWindowMetadata {
    title: Option<String>,
    class: Option<String>,
}

/// Database time recording
mod database;
use database::{CategoryDurationCounter, Database, DatabaseTime};

/// Xcb interface
mod xcb_stalker;
use xcb_stalker::ActiveWindowChanges;

/** Classifier: stores rules used to determine categories for time spent.
 * Rules are stored in an ordered list.
 * The first matching rule in the list chooses the category.
 * A category can appear in multiple rules.
 */
struct Classifier {
    filters: Vec<(String, Box<Fn(&ActiveWindowMetadata) -> bool>)>,
}

impl Classifier {
    /// Create a new classifier with no rules.
    fn new() -> Self {
        Classifier {
            filters: Vec::new(),
        }
    }
    /// Add a rule at the end of the list, for the given category.
    fn append_filter<F>(&mut self, category: &str, filter: F)
    where
        F: 'static + Fn(&ActiveWindowMetadata) -> bool,
    {
        self.filters
            .push((String::from(category), Box::new(filter)));
    }
    /// Return the list of all defined categories, unique.
    fn categories(&self) -> Vec<&str> {
        let mut categories: Vec<&str> = self.filters
            .iter()
            .map(|(category, _)| category.as_str())
            .collect();
        categories.sort();
        categories.dedup();
        categories
    }
    /// Determine the category for the given window metadata.
    fn classify(&self, metadata: &ActiveWindowMetadata) -> Option<&str> {
        self.filters
            .iter()
            .find(|(_category, filter)| filter(metadata))
            .map(|(category, _filter)| category.as_str())
    }
    // TODO read rules from simple language ?
}

fn write_durations_to_disk(
    db: &mut Database,
    duration_counter: &CategoryDurationCounter,
    window_start: &DatabaseTime,
) -> io::Result<()> {
    println!("Write to disk");

    db.rewrite_last_entry(window_start, duration_counter.durations())
}

fn change_time_window(
    db: &mut Database,
    duration_counter: &mut CategoryDurationCounter,
    window_start: &mut DatabaseTime,
    time_window_size: time::Duration,
) -> io::Result<()> {
    // Flush current durations values
    write_durations_to_disk(db, duration_counter, window_start)?;
    // Create a new time window
    db.lock_last_entry();
    duration_counter.reset_durations();
    *window_start = *window_start + chrono::Duration::from_std(time_window_size).unwrap();
    Ok(())
}

fn run_daemon(
    classifier: Classifier,
    db_file: &Path,
    db_write_interval: time::Duration,
    time_window_size: time::Duration,
) -> Result<(), String> {
    // Setup state
    let mut db = Database::open(db_file, classifier.categories())
        .map_err(|e| format!("Unable to open database '{}':\n{}", db_file.display(), e))?;
    let mut duration_counter = CategoryDurationCounter::new(db.categories());
    let active_window_changes =
        ActiveWindowChanges::new().map_err(|e| format!("Unable to start event listener:\n{}", e))?;

    // Determine current time window
    let now = DatabaseTime::from(time::SystemTime::now());
    let window_start = {
        if let Some((time, durations)) = db.get_last_entry()
            .map_err(|e| format!("Unable to read last database entry:\n{}", e))?
        {
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
        let initial_metadata = active_window_changes
            .get_current_metadata()
            .map_err(|e| format!("Unable to get window metadata:\n{}", e))?;
        let initial_category = classifier.classify(&initial_metadata);
        duration_counter.category_changed(initial_category);
    }

    // Wrap shared state in RefCell: cannot prove with type that mutations are exclusive.
    let db = RefCell::new(db);
    let duration_counter = RefCell::new(duration_counter);
    let window_start = RefCell::new(window_start);

    // Listen to active window changes.
    let all_category_changes = active_window_changes
        .map_err(|e| format!("Window metadata listener failed:\n{}", e))
        .for_each(|active_window| {
            println!("task_handle_window_change");
            let category = classifier.classify(&active_window);
            duration_counter.borrow_mut().category_changed(category);
            Ok(())
        });

    // Periodically write database to file
    let all_db_writes =
        tokio::timer::Interval::new(time::Instant::now() + db_write_interval, db_write_interval)
            .map_err(|e| format!("Timer error: {}", e))
            .for_each(|_instant| {
                println!("task_write_db");
                write_durations_to_disk(
                    &mut db.borrow_mut(),
                    &duration_counter.borrow(),
                    &window_start.borrow(),
                ).map_err(|e| format!("Failed to write database file: {}", e))
            });

    // Periodically change time window
    let all_time_window_changes = tokio::timer::Interval::new(
        time::Instant::now() + duration_to_next_window_change,
        time_window_size,
    ).map_err(|e| format!("Timer error: {}", e))
        .for_each(|_instant| {
            println!("task_new_time_window");
            change_time_window(
                &mut db.borrow_mut(),
                &mut duration_counter.borrow_mut(),
                &mut window_start.borrow_mut(),
                time_window_size,
            ).map_err(|e| format!("Failed to change the time window: {}", e))
        });

    // Create a tokio runtime to implement an event loop.
    // Single threaded is enough.
    // TODO support signals using tokio_signal crate ?
    let mut runtime = tokio::runtime::current_thread::Runtime::new()
        .map_err(|e| format!("Unable to create tokio runtime:\n{}", e))?;
    runtime
        .block_on(all_category_changes.join3(all_db_writes, all_time_window_changes))
        .map(|(_, _, _)| ())
}

fn main() -> Result<(), DebugAsDisplay<String>> {
    // Config TODO from args
    let time_window_size = time::Duration::from_secs(3600);
    let db_write_interval = time::Duration::from_secs(10);

    // Setup test classifier
    let mut classifier = Classifier::new();
    classifier.append_filter(&"coding", |md| {
        md.class
            .as_ref()
            .map(|class| class == "konsole")
            .unwrap_or(false)
    });
    classifier.append_filter(&"unknown", |_| true);

    run_daemon(
        classifier,
        Path::new("test"),
        db_write_interval,
        time_window_size,
    ).map_err(|err| DebugAsDisplay(err))
}

/** If main returns Result<_, E>, E will be printed with fmt::Debug.
 * By wrapping T in this structure, it will be printed nicely with fmt::Display.
 */
struct DebugAsDisplay<T>(T);
impl<T: fmt::Display> fmt::Debug for DebugAsDisplay<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}
