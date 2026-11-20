//! Keep physical input alive across transient HID errors and profile changes.
use super::*;

const POLL: Duration = Duration::from_millis(750);

struct Monitor {
    previous: Option<(u16, u8)>,
    next_poll: Instant,
    retry_at: Option<Instant>,
    failures: u32,
}

impl Monitor {
    fn new(now: Instant) -> Self {
        Self {
            previous: None,
            next_poll: now,
            retry_at: None,
            failures: 0,
        }
    }

    fn tick(&mut self, w: &mut Wire, now: Instant) {
        if self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        let recovering = self.retry_at.is_some();
        match self.receive(w, now, recovering) {
            Ok(()) => {
                if recovering {
                    eprintln!(
                        "[logitech] button monitor recovered after {} failure(s)",
                        self.failures
                    );
                    // Refresh even if the profile number stayed the same: events
                    // may have been lost while the connection was unavailable.
                    if let Some(cb) = &w.notify {
                        cb(HubEvent::StateChanged(w.id.clone()));
                    }
                }
                self.retry_at = None;
                self.failures = 0;
            }
            Err(e) => {
                w.button_activity(0);
                if let Some(host) = &mut w.host {
                    host.invalidate();
                }
                if self.failures == 0 {
                    eprintln!("[logitech] button monitor retrying: {e}");
                }
                self.failures = self.failures.saturating_add(1);
                let delay = Duration::from_millis(250 * (1u64 << self.failures.min(3)));
                // A request can spend two seconds waiting for a reply. Back off
                // from its completion, not from the beginning of that wait.
                self.retry_at = Some(now.max(Instant::now()) + delay);
            }
        }
    }

    fn receive(&mut self, w: &mut Wire, now: Instant, recovering: bool) -> HubResult<()> {
        // Requests and idle reads share this lock. Wire::call also dispatches
        // notifications, including releases arriving during profile reads.
        for _ in 0..64 {
            match w.channel.recv(0) {
                Ok(r) => w.notification(&r),
                Err(HubError::Protocol(_)) => break, // Empty nonblocking read.
                Err(e) => return Err(e),
            }
        }
        if !recovering && now < self.next_poll {
            return Ok(());
        }
        if recovering && w.spy != 0 {
            w.call(w.spy, 1, &[])?;
        }
        let value = w
            .onboard(4, &[])
            .and_then(|p| w.onboard(11, &[]).map(|d| (be(&p), d[0])))?;
        if !(1..=u16::from(w.count)).contains(&value.0) {
            return Err(protocol("Unknown active profile in button monitor"));
        }
        let changed = self.previous.is_some_and(|old| old != value);
        let profile_changed = self.previous.is_some_and(|old| old.0 != value.0);
        if profile_changed && !recovering && w.spy != 0 {
            w.call(w.spy, 1, &[])?;
        }
        if w.host.as_ref().is_some_and(|h| h.profile != value.0 as u8) {
            w.refresh_host(value.0 as u8)?;
        }
        if changed && !recovering {
            if let Some(cb) = &w.notify {
                cb(HubEvent::StateChanged(w.id.clone()));
            }
        }
        self.previous = Some(value);
        self.next_poll = now + POLL;
        Ok(())
    }
}

pub(super) fn spawn(wire: &Arc<Mutex<Wire>>) {
    let weak = Arc::downgrade(wire);
    std::thread::spawn(move || {
        let mut monitor = Monitor::new(Instant::now());
        loop {
            std::thread::sleep(Duration::from_millis(5));
            let Some(shared) = weak.upgrade() else { break };
            let Ok(mut w) = shared.try_lock() else {
                continue;
            };
            // A missed reply must not permanently kill notification reception.
            // A genuinely disconnected device is dropped by backend enumeration.
            monitor.tick(&mut w, Instant::now());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(w: &Wire, feature: u8, address: u8, args: &[u8]) {
        let request = [0x11, 0xff, feature, address];
        let mut response = [0; 20];
        response[..4].copy_from_slice(&request);
        response[4..4 + args.len()].copy_from_slice(args);
        w.channel.mock_exchange(&request, &response);
    }

    #[test]
    fn timeout_or_io_error_recovers_and_keeps_receiving_after_idle() {
        for failure in ["timeout", "write", "read"] {
            let now = Instant::now();
            let mut w = super::super::tests::mock_wire();
            w.spy = 13;
            w.button_mask = 2;
            let observed = Arc::new(Mutex::new(Vec::new()));
            let out = observed.clone();
            w.notify = Some(Arc::new(move |ev| out.lock().unwrap().push(ev)));
            let mut m = Monitor::new(now);
            match failure {
                "write" => w.channel.mock_fail_send(&[0x11, 0xff, 12]),
                "read" => w.channel.mock_fail_recv(),
                _ => {}
            }
            m.tick(&mut w, now); // Poll fails; previously the thread ended here.
            assert_eq!(m.failures, 1);
            assert_eq!(w.button_mask, 0);
            assert!(matches!(&observed.lock().unwrap()[0],
                HubEvent::ButtonActivity { pressed, .. } if pressed.is_empty()));
            let sent = w.channel.mock_sent().len();
            m.tick(&mut w, now + Duration::from_millis(100));
            assert_eq!(w.channel.mock_sent().len(), sent); // Bounded retries.
            let mut sw = w.software;
            for (feature, function, args) in [(13, 1, &[][..]), (12, 4, &[0, 5]), (12, 11, &[0])] {
                sw = sw % 15 + 1;
                exchange(&w, feature, function << 4 | sw, args);
            }
            let recovered = m.retry_at.unwrap();
            m.tick(&mut w, recovered);
            assert_eq!(m.failures, 0);
            assert!(m.retry_at.is_none());
            assert_eq!(m.previous, Some((5, 0)));
            assert!(observed
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, HubEvent::StateChanged(_))));
            // The next press/release is consumed without any settings read.
            w.channel.mock_reply(&[0x11, 0xff, 13, 0, 1, 0]);
            w.channel.mock_reply(&[0x11, 0xff, 13, 0, 0, 0]);
            m.tick(&mut w, recovered + Duration::from_millis(5));
            let held: Vec<_> = observed
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| {
                    if let HubEvent::ButtonActivity { pressed, .. } = e {
                        Some(pressed.clone())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(held, vec![vec![], vec![8], vec![]]);
            w.spy = 0;
        }
    }

    #[test]
    fn profile_change_rearms_spy_but_dpi_change_does_not() {
        let now = Instant::now();
        let mut w = super::super::tests::mock_wire();
        w.spy = 13;
        let mut m = Monitor::new(now);
        m.previous = Some((4, 0));
        exchange(&w, 12, 0x4d, &[0, 5]);
        exchange(&w, 12, 0xbe, &[0]);
        exchange(&w, 13, 0x1f, &[]);
        m.tick(&mut w, now);
        assert_eq!(m.previous, Some((5, 0)));
        assert_eq!(w.channel.mock_sent().len(), 3);
        exchange(&w, 12, 0x41, &[0, 5]);
        exchange(&w, 12, 0xb2, &[1]);
        m.tick(&mut w, now + POLL);
        assert_eq!(m.previous, Some((5, 1)));
        assert_eq!(w.channel.mock_sent().len(), 5);
        w.spy = 0;
    }
}
