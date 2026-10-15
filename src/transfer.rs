//! The queue of files to save. Pure logic: the view renders it, a worker drives it.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Saving,
}

#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub state: State,
    /// Bytes done. `None` until the first progress report.
    done: Option<u64>,
}

impl Transfer {
    /// Percent done, from 0 to 100. An empty file is at 100 after its progress report.
    pub fn percent(&self) -> u8 {
        match self.done {
            None => 0,
            Some(_) if self.size == 0 => 100,
            Some(done) => (done.min(self.size) * 100 / self.size) as u8,
        }
    }

    /// Progress units: an empty file counts as one unit, so that it has a weight.
    fn units(&self) -> (u64, u64) {
        let total = self.size.max(1);
        let done = match self.done {
            None => 0,
            Some(_) if self.size == 0 => 1,
            Some(done) => done.min(self.size),
        };
        (done, total)
    }
}

/// The counters restart when a file is added to an empty queue.
#[derive(Default)]
pub struct Queue {
    transfers: Vec<Transfer>,
    next_id: u64,
    added: usize,
    finished: usize,
    succeeded: usize,
    total_units: u64,
    finished_units: u64,
}

impl Queue {
    pub fn add(&mut self, path: PathBuf, name: String, size: u64) {
        if self.transfers.is_empty() {
            self.added = 0;
            self.finished = 0;
            self.succeeded = 0;
            self.total_units = 0;
            self.finished_units = 0;
        }
        self.next_id += 1;
        let transfer = Transfer {
            id: self.next_id,
            path,
            name,
            size,
            state: State::Waiting,
            done: None,
        };
        self.added += 1;
        self.total_units += transfer.units().1;
        self.transfers.push(transfer);
    }

    /// Starts the first waiting transfer and returns its id and path.
    /// Returns `None` if a transfer is active or no transfer waits.
    pub fn start_next(&mut self) -> Option<(u64, PathBuf)> {
        if self.active().is_some() {
            return None;
        }
        let next = self
            .transfers
            .iter_mut()
            .find(|t| t.state == State::Waiting)?;
        next.state = State::Saving;
        Some((next.id, next.path.clone()))
    }

    /// Sets the bytes done of the active transfer. Returns true if its percent changed.
    pub fn set_progress(&mut self, done: u64) -> bool {
        let Some(active) = self.transfers.iter_mut().find(|t| t.state == State::Saving) else {
            return false;
        };
        let before = (active.done.is_some(), active.percent());
        active.done = Some(done);
        before != (true, active.percent())
    }

    pub fn complete(&mut self) {
        if self.finish() {
            self.succeeded += 1;
        }
    }

    pub fn fail(&mut self) {
        self.finish();
    }

    /// Removes the active transfer. Returns false if no transfer is active.
    fn finish(&mut self) -> bool {
        let Some(index) = self.transfers.iter().position(|t| t.state == State::Saving) else {
            return false;
        };
        let transfer = self.transfers.remove(index);
        self.finished += 1;
        self.finished_units += transfer.units().1;
        true
    }

    fn active(&self) -> Option<&Transfer> {
        self.transfers.iter().find(|t| t.state == State::Saving)
    }

    pub fn transfers(&self) -> &[Transfer] {
        &self.transfers
    }

    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
    }

    /// (n, total) for "Saving n of total".
    pub fn position(&self) -> Option<(usize, usize)> {
        (!self.is_empty()).then_some((self.finished + 1, self.added))
    }

    /// Percent of all bytes added since the queue was last empty.
    pub fn overall_percent(&self) -> u8 {
        if self.total_units == 0 {
            return 0;
        }
        let active = self.active().map_or(0, |t| t.units().0);
        ((self.finished_units + active) * 100 / self.total_units) as u8
    }

    /// Files saved with success since the queue was last empty.
    pub fn succeeded(&self) -> usize {
        self.succeeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_with(sizes: &[u64]) -> Queue {
        let mut queue = Queue::default();
        for (i, size) in sizes.iter().enumerate() {
            queue.add(PathBuf::from(format!("/f{i}")), format!("f{i}"), *size);
        }
        queue
    }

    #[test]
    fn empty_queue_has_no_position_and_nothing_to_start() {
        let mut queue = Queue::default();

        assert!(queue.is_empty());
        assert_eq!(queue.position(), None);
        assert_eq!(queue.overall_percent(), 0);
        assert!(queue.start_next().is_none());
    }

    #[test]
    fn add_puts_files_at_the_end_in_the_waiting_state() {
        let queue = queue_with(&[10, 20]);

        let names: Vec<_> = queue.transfers().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["f0", "f1"]);
        assert!(queue.transfers().iter().all(|t| t.state == State::Waiting));
        assert_eq!(queue.position(), Some((1, 2)));
    }

    #[test]
    fn start_next_starts_one_transfer_at_a_time() {
        let mut queue = queue_with(&[10, 20]);

        let started = queue.start_next().unwrap();

        assert_eq!(started, (queue.transfers()[0].id, PathBuf::from("/f0")));
        assert_eq!(queue.transfers()[0].state, State::Saving);
        assert!(queue.start_next().is_none());
    }

    #[test]
    fn set_progress_changes_the_percent_of_the_active_transfer() {
        let mut queue = queue_with(&[200]);
        queue.start_next();

        assert!(queue.set_progress(50));
        assert_eq!(queue.transfers()[0].percent(), 25);
        // Same percent: no change to show.
        assert!(!queue.set_progress(51));
        assert!(queue.set_progress(200));
        assert_eq!(queue.transfers()[0].percent(), 100);
    }

    #[test]
    fn complete_removes_the_transfer_and_counts_the_success() {
        let mut queue = queue_with(&[10, 30]);
        queue.start_next();
        queue.set_progress(10);

        queue.complete();

        assert_eq!(queue.transfers().len(), 1);
        assert_eq!(queue.succeeded(), 1);
        assert_eq!(queue.position(), Some((2, 2)));
        assert_eq!(queue.overall_percent(), 25);
    }

    #[test]
    fn failed_transfer_leaves_the_queue_but_is_not_a_success() {
        let mut queue = queue_with(&[10, 10]);
        queue.start_next();

        queue.fail();

        assert_eq!(queue.transfers().len(), 1);
        assert_eq!(queue.succeeded(), 0);
        assert_eq!(queue.position(), Some((2, 2)));
        assert_eq!(queue.overall_percent(), 50);
    }

    #[test]
    fn files_added_during_a_save_extend_the_total() {
        let mut queue = queue_with(&[100]);
        queue.start_next();
        queue.set_progress(50);

        queue.add(PathBuf::from("/late"), "late".into(), 100);

        assert_eq!(queue.transfers()[1].name, "late");
        assert_eq!(queue.position(), Some((1, 2)));
        assert_eq!(queue.overall_percent(), 25);
    }

    #[test]
    fn counters_restart_after_the_queue_becomes_empty() {
        let mut queue = queue_with(&[10]);
        queue.start_next();
        queue.complete();
        assert!(queue.is_empty());
        assert_eq!(queue.succeeded(), 1);

        queue.add(PathBuf::from("/next"), "next".into(), 10);

        assert_eq!(queue.succeeded(), 0);
        assert_eq!(queue.position(), Some((1, 1)));
        assert_eq!(queue.overall_percent(), 0);
    }

    #[test]
    fn empty_file_is_at_100_percent_only_after_its_progress_report() {
        let mut queue = queue_with(&[0]);
        assert_eq!(queue.transfers()[0].percent(), 0);
        queue.start_next();
        assert_eq!(queue.transfers()[0].percent(), 0);

        assert!(queue.set_progress(0));

        assert_eq!(queue.transfers()[0].percent(), 100);
        assert_eq!(queue.overall_percent(), 100);
    }
}
