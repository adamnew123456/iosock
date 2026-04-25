use std::{
    io,
    os::fd::{AsRawFd, RawFd},
};

/// A polled file descriptor is a file descriptor registered with epoll. It
/// tracks the configuration last provided to epoll, as well as requests made by
/// the channel manager to start/stop waiting for different events.
pub struct PolledFd {
    fd: RawFd,
    event: epoll::Event,
    new_events: epoll::Events,
}

impl PolledFd {
    /// Creates a new polled file descriptor from its initial epoll
    /// configuration.
    pub fn new(fd: RawFd, event: epoll::Event) -> PolledFd {
        PolledFd {
            fd,
            event,
            new_events: epoll::Events::from_bits_truncate(event.events),
        }
    }

    /// Requests a change to the read polling flag. If true the file descriptor
    /// will start polling for reads, if false it will stop.
    pub fn poll_for_read(&mut self, poll_reads: bool) {
        self.new_events.set(epoll::Events::EPOLLIN, poll_reads)
    }

    /// Requests a change to the write polling flag. If true the file descriptor
    /// will start polling for writes, if false it will stop.
    pub fn poll_for_write(&mut self, poll_writes: bool) {
        self.new_events.set(epoll::Events::EPOLLOUT, poll_writes)
    }

    /// Applies the new event mask to the stored event. If the stored event's
    /// event mask is the same as the new events, this returns [`None`] because
    /// epoll does not need to be updated. If changes are required, this returns
    /// a copy of the stored [`epoll::Event`] with new event flags.
    pub fn apply_updates(&mut self) -> Option<epoll::Event> {
        if self.event.events == self.new_events.bits() {
            None
        } else {
            self.event.events = self.new_events.bits();
            Some(self.event.clone())
        }
    }
}

/// Maintains a set of [`PolledFd`] values. The [`ChannelManager`] uses this to
/// register new file descriptors and change the registrations on existing file
/// descriptors, while the [`Pollster`] processes these requests and configures
/// epoll as required.
pub struct PolledFdSet {
    existing: Vec<PolledFd>,
    new: Vec<(RawFd, epoll::Events)>,
    removed: Vec<RawFd>,
}

impl PolledFdSet {
    /// Creates an empty file descriptor set.
    pub fn new() -> PolledFdSet {
        PolledFdSet {
            existing: Vec::new(),
            new: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// For use by the [`ChannelManager`]. Adds a new file descriptor for
    /// polling. This only notifies the pollster that the channel manager is
    /// interested in this file descriptor, it is up to the pollster to pass
    /// this file descrpitor to epoll.
    ///
    /// Panics if the provided file descriptor was previously registered.
    pub fn register<F: AsRawFd>(&mut self, to_fd: &F, events: epoll::Events) {
        let fd = to_fd.as_raw_fd();

        // Check that this doesn't duplicate any file descriptor previously
        // registered, both processed and unprocessed
        assert!(
            {
                let has_existing = self.existing.iter().position(|pf| pf.fd == fd).is_some();
                !has_existing
            },
            "{fd} is already registered in the pollster"
        );

        assert!(
            {
                let has_new = self.new.iter().position(|(nfd, _)| *nfd == fd).is_some();
                !has_new
            },
            "{fd} is already in the new set"
        );

        self.new.push((fd, events));
    }

    /// For use by [`ChannelManager`]. Passes the file descriptor to the
    /// function for modification.
    ///
    /// Panics if the provided file dscriptor was not previously registered and
    /// processed by the pollster. This cannot be used to modify unprocessed file
    /// descriptors passed to [`register`].
    pub fn modify<F: AsRawFd, M: FnOnce(&mut PolledFd)>(&mut self, to_fd: &F, modify_fn: M) {
        let fd = to_fd.as_raw_fd();
        let existing = self.existing.iter_mut().filter(|pf| pf.fd == fd).next();
        assert!(existing.is_some(), "{fd} not registered in the pollster");
        modify_fn(existing.unwrap())
    }

    /// For use by [`ChannelManager`]. Removes the file descriptor from the
    /// existing file descriptor set.
    ///
    /// This does not prevent the pollster from passing already dequeued events
    /// onto the channel manager. For example, if a read and a hangup event are
    /// both pending on this file descriptor and the read event invokes
    /// [`unregister`], the hangup event is still processed since epoll
    /// coalesced both states into a single event flag.
    ///
    /// Panics if the provided file dscriptor was not previously registered and
    /// processed by the pollster. This cannot be used to remove unprocessed
    /// file descriptors passed to [`register`].
    pub fn unregister<F: AsRawFd>(&mut self, to_fd: &F) {
        let fd = to_fd.as_raw_fd();
        let existing = self.existing.iter_mut().position(|pf| pf.fd == fd);
        assert!(existing.is_some(), "{fd} not registered in the pollster");
        self.existing.remove(existing.unwrap());
        self.removed.push(fd);
    }

    /// For use by [`ChannelManager`]. Removes the file descriptor from the
    /// existing file descriptor set.
    ///
    /// Like [`unregister`] but intended for cases where the file descriptor
    /// itself will be closed. In these situations the pollster cannot do
    /// anything, since in the best case this will trigger EBADF, and in the
    /// worst case will close the wrong file descriptor if the file descriptor
    /// is reused.
    pub fn unregister_closed<F: AsRawFd>(&mut self, to_fd: &F) {
        let fd = to_fd.as_raw_fd();
        let existing = self.existing.iter_mut().position(|pf| pf.fd == fd);
        assert!(existing.is_some(), "{fd} not registered in the pollster");
        self.existing.remove(existing.unwrap());
    }

    /// For use by [`Pollster`]. File descriptors given to [`register`] are
    /// passed to the new file descriptor callback, while file descriptors
    /// changed by [`modify`] are given to the update callback, and file
    /// descriptors passed to [`unregister`] are given to the delete callback.
    /// After this returns, all file descriptors are assumed to be known by the
    /// pollster.
    ///
    /// This stops the first time any callback returns an [`Err`]. There is no
    /// mechanism to recover from this; an epoll failure is assumed to be fatal.
    pub fn apply_updates<NF, UF, DF>(
        &mut self,
        mut new_fn: NF,
        mut update_fn: UF,
        mut delete_fn: DF,
    ) -> io::Result<()>
    where
        NF: FnMut(RawFd, epoll::Events) -> io::Result<PolledFd>,
        UF: FnMut(RawFd, epoll::Event) -> io::Result<()>,
        DF: FnMut(RawFd) -> io::Result<()>,
    {
        for pfd in self.existing.iter_mut() {
            if let Some(new_event) = pfd.apply_updates() {
                update_fn(pfd.fd, new_event)?;
            }
        }

        for (fd, events) in self.new.drain(..) {
            self.existing.push(new_fn(fd, events)?);
        }

        for fd in self.removed.drain(..) {
            delete_fn(fd)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    struct FdSetUpdates {
        pub new: Vec<(RawFd, epoll::Events)>,
        pub updates: Vec<(RawFd, epoll::Event)>,
        pub removed: Vec<RawFd>,
    }

    impl FdSetUpdates {
        fn new() -> Self {
            FdSetUpdates {
                new: Vec::new(),
                updates: Vec::new(),
                removed: Vec::new(),
            }
        }

        fn apply_updates(&mut self, fd_set: &mut PolledFdSet) -> io::Result<()> {
            fd_set.apply_updates(
                |fd, events| {
                    self.new.push((fd, events));
                    let event = epoll::Event::new(events, 0);
                    Ok(PolledFd::new(fd, event))
                },
                |fd, event| {
                    self.updates.push((fd, event.clone()));
                    Ok(())
                },
                |fd| {
                    self.removed.push(fd);
                    Ok(())
                },
            )
        }

        fn reset(&mut self) {
            self.new.clear();
            self.updates.clear();
            self.removed.clear();
        }
    }

    #[test]
    fn new_pfd_has_no_updates() {
        let event = epoll::Event::new(epoll::Events::empty(), 0);
        let mut pfd = PolledFd::new(0, event);
        assert_eq!(None, pfd.apply_updates());
    }

    #[test]
    fn enable_write_polling_sets_epollout() {
        let event = epoll::Event::new(epoll::Events::empty(), 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_write(true);

        let new_events = epoll::Event::new(epoll::Events::EPOLLOUT, 0);
        assert_eq!(Some(new_events), pfd.apply_updates());
    }

    #[test]
    fn disable_write_polling_clears_epollout() {
        let event = epoll::Event::new(epoll::Events::EPOLLOUT, 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_write(false);

        let new_events = epoll::Event::new(epoll::Events::empty(), 0);
        assert_eq!(Some(new_events), pfd.apply_updates());
    }

    #[test]
    fn enable_write_polling_noop() {
        let event = epoll::Event::new(epoll::Events::EPOLLOUT, 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_write(true);
        assert_eq!(None, pfd.apply_updates());
    }

    #[test]
    fn disable_write_polling_noop() {
        let event = epoll::Event::new(epoll::Events::empty(), 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_write(false);
        assert_eq!(None, pfd.apply_updates());
    }

    #[test]
    fn enable_read_polling_sets_epollin() {
        let event = epoll::Event::new(epoll::Events::empty(), 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_read(true);

        let new_events = epoll::Event::new(epoll::Events::EPOLLIN, 0);
        assert_eq!(Some(new_events), pfd.apply_updates());
    }

    #[test]
    fn disable_read_polling_clears_epollin() {
        let event = epoll::Event::new(epoll::Events::EPOLLIN, 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_read(false);

        let new_events = epoll::Event::new(epoll::Events::empty(), 0);
        assert_eq!(Some(new_events), pfd.apply_updates());
    }

    #[test]
    fn enable_read_polling_noop() {
        let event = epoll::Event::new(epoll::Events::EPOLLIN, 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_read(true);
        assert_eq!(None, pfd.apply_updates());
    }

    #[test]
    fn disable_read_polling_noop() {
        let event = epoll::Event::new(epoll::Events::empty(), 0);
        let mut pfd = PolledFd::new(0, event);
        pfd.poll_for_read(false);
        assert_eq!(None, pfd.apply_updates());
    }

    #[test]
    fn fdset_empty_apply_updates() {
        let mut fd_set = PolledFdSet::new();
        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        assert_eq!(0, updates.new.len());
        assert_eq!(0, updates.updates.len());
        assert_eq!(0, updates.removed.len());
    }

    #[test]
    fn fdset_register_passed_to_newfn() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);

        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        assert_eq!(vec![(0, epoll::Events::EPOLLIN)], updates.new);
        assert_eq!(0, updates.updates.len());
    }

    #[test]
    fn fdset_register_multiple() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);
        fd_set.register(&1, epoll::Events::EPOLLOUT);

        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        assert_eq!(
            vec![(0, epoll::Events::EPOLLIN), (1, epoll::Events::EPOLLOUT)],
            updates.new
        );
        assert_eq!(0, updates.updates.len());
    }

    #[test]
    fn fdset_modify_passed_to_updatefn() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);

        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        updates.reset();

        fd_set.modify(&0, |pfd| pfd.poll_for_write(true));
        updates.apply_updates(&mut fd_set).unwrap();

        let mut new_events = epoll::Events::EPOLLIN;
        new_events.set(epoll::Events::EPOLLOUT, true);
        let new_event = epoll::Event::new(new_events, 0);

        assert_eq!(0, updates.new.len());
        assert_eq!(vec![(0, new_event)], updates.updates);
    }

    #[test]
    fn fdset_modify_nochange() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);

        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        updates.reset();

        fd_set.modify(&0, |pfd| pfd.poll_for_read(true));
        updates.apply_updates(&mut fd_set).unwrap();

        assert_eq!(0, updates.new.len());
        assert_eq!(0, updates.updates.len());
    }

    #[test]
    fn fdset_unregister_passed_to_deletefn() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);

        let mut updates = FdSetUpdates::new();
        updates.apply_updates(&mut fd_set).unwrap();
        updates.reset();

        fd_set.unregister(&0);
        updates.apply_updates(&mut fd_set).unwrap();

        assert_eq!(0, updates.new.len());
        assert_eq!(0, updates.updates.len());
        assert_eq!(vec![0], updates.removed);

        fd_set.register(&0, epoll::Events::EPOLLIN);
    }

    #[test]
    #[should_panic]
    fn fdset_duplicate_register_panics() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);
        fd_set.register(&0, epoll::Events::EPOLLIN);
    }

    #[test]
    #[should_panic]
    fn fdset_register_after_updates_panics() {
        let mut fd_set = PolledFdSet::new();
        let mut updates = FdSetUpdates::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);
        updates.apply_updates(&mut fd_set).unwrap();

        fd_set.register(&0, epoll::Events::EPOLLIN);
    }

    #[test]
    #[should_panic]
    fn fdset_unregister_nonexisting_panics() {
        let mut fd_set = PolledFdSet::new();
        fd_set.unregister(&1);
    }

    #[test]
    #[should_panic]
    fn fdset_unregister_just_registered_panics() {
        let mut fd_set = PolledFdSet::new();
        fd_set.register(&0, epoll::Events::EPOLLIN);
        fd_set.unregister(&0);
    }
}
