//! Supervision written against `Instance`, not against a pid: the health
//! gate and the patient stop. Both were inlined in `run.rs` for years and
//! untestable; the fake below is the first test double the runtime has.

use std::time::{Duration, Instant};

use nix::sys::signal::Signal;

use crate::runtime::backend::Instance;

/// The deploy health gate's verdict.
pub enum Health {
    Healthy,
    /// The instance died inside the grace window.
    Died,
    /// A `[health] port` never answered within grace; the last connect
    /// error, if any attempt was made.
    NoAnswer(Option<std::io::Error>),
}

/// The deploy health gate. With a port: a TCP connect within `grace`.
/// Without: the instance just has to be alive after the window.
pub fn health_gate(
    instance: &dyn Instance,
    port: Option<u16>,
    grace: Duration,
    poll: Duration,
) -> Health {
    let deadline = Instant::now() + grace;
    let mut last_err: Option<std::io::Error> = None;
    loop {
        if !instance.alive() {
            return Health::Died;
        }
        if let Some(port) = port {
            match instance.tcp_open(port, Duration::from_millis(300)) {
                Ok(()) => return Health::Healthy,
                Err(e) => last_err = Some(e),
            }
        }
        if Instant::now() >= deadline {
            return match port {
                Some(_) => Health::NoAnswer(last_err),
                None => Health::Healthy,
            };
        }
        std::thread::sleep(poll);
    }
}

/// Deliberate stop: `stop`, up to `patience` to comply, then SIGKILL.
/// Returns the exit code once the instance is reaped; `None` only if even
/// SIGKILL did not end it within five seconds.
pub fn stop_with_patience(
    instance: &mut dyn Instance,
    stop: Signal,
    patience: Duration,
    poll: Duration,
) -> Option<i32> {
    let _ = instance.signal(stop);
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(code)) = instance.try_wait() {
            return Some(code);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(poll);
    }
    let _ = instance.signal(Signal::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(code)) = instance.try_wait() {
            return Some(code);
        }
        if Instant::now() >= kill_deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

/// Ask every instance the signal HANDLER could not reach to stop, and say
/// how many that was.
///
/// The handler stops an instance by `kill`ing `child_pid()` — the only thing
/// that works while the main loop is blocked in the `--after` wait, so
/// namespace instances are already covered by the time this runs. A microVM
/// has no child process at all: its `child_pid()` is `None`, and the only
/// way in is `Instance::signal`, which reaches the guest init over the
/// control channel. Without this call such an instance received **nothing**
/// until `SHUTDOWN_GRACE` expired and was then SIGKILLed — `^C` on `ply run`
/// took ten seconds and returned 255 instead of the app's own exit code, and
/// a database lost its clean shutdown.
///
/// An instance the handler already reached is skipped rather than signalled
/// again: sending a second SIGTERM to an app that asked for one — nginx's
/// SIGQUIT drain, httpd's SIGWINCH — is not what a polite stop promises.
///
/// Called ONCE, on the loop's first observation of the shutdown.
pub fn request_stop(instances: &[&dyn Instance], stop: Signal) -> usize {
    let mut sent = 0;
    for instance in instances.iter().filter(|i| i.child_pid().is_none()) {
        let _ = instance.signal(stop);
        sent += 1;
    }
    sent
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeInstance {
    pub polls: std::cell::Cell<u32>,
    /// From this poll on the instance is dead (exit `exit`).
    pub dies_at_poll: Option<u32>,
    pub exit: i32,
    /// From this poll on `tcp_open` succeeds; `None` = never.
    pub answers_from_poll: Option<u32>,
    /// A signal it complies with (ends with `exit`); SIGKILL always ends it (137).
    pub obeys: Option<Signal>,
    /// `Some` = a real child process the signal handler can `kill` itself;
    /// `None` (the default) = a microVM, which only the main loop can reach.
    pub child_pid: Option<i32>,
    pub signals: std::cell::RefCell<Vec<Signal>>,
    ended: std::cell::Cell<Option<i32>>,
}

#[cfg(test)]
impl FakeInstance {
    fn tick(&self) -> Option<i32> {
        self.polls.set(self.polls.get() + 1);
        if self.ended.get().is_none() {
            if let Some(at) = self.dies_at_poll {
                if self.polls.get() >= at {
                    self.ended.set(Some(self.exit));
                }
            }
        }
        self.ended.get()
    }
}

#[cfg(test)]
impl Instance for FakeInstance {
    fn pid(&self) -> i32 {
        4242
    }
    fn child_pid(&self) -> Option<i32> {
        self.child_pid
    }
    fn ip(&self) -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::new(10, 77, 0, 2)
    }
    fn alive(&self) -> bool {
        self.tick().is_none()
    }
    fn signal(&self, sig: Signal) -> crate::Result<()> {
        self.signals.borrow_mut().push(sig);
        if sig == Signal::SIGKILL {
            self.ended.set(Some(137));
        } else if self.obeys == Some(sig) {
            self.ended.set(Some(self.exit));
        }
        Ok(())
    }
    fn try_wait(&mut self) -> crate::Result<Option<i32>> {
        Ok(self.tick())
    }
    fn tcp_open(&self, _port: u16, _timeout: Duration) -> std::io::Result<()> {
        match self.answers_from_poll {
            Some(at) if self.polls.get() >= at => Ok(()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_millis(1);
    const GRACE: Duration = Duration::from_millis(200);

    #[test]
    fn dying_during_grace_is_reported_not_retried() {
        let fake = FakeInstance {
            dies_at_poll: Some(2),
            answers_from_poll: Some(5),
            ..Default::default()
        };
        assert!(matches!(
            health_gate(&fake, Some(5432), GRACE, POLL),
            Health::Died
        ));
    }

    #[test]
    fn a_port_that_answers_is_healthy_as_soon_as_it_does() {
        let fake = FakeInstance {
            answers_from_poll: Some(3),
            ..Default::default()
        };
        let began = Instant::now();
        assert!(matches!(
            health_gate(&fake, Some(5432), GRACE, POLL),
            Health::Healthy
        ));
        assert!(fake.polls.get() >= 3);
        assert!(
            began.elapsed() < GRACE,
            "healthy must not wait out the grace window"
        );
    }

    #[test]
    fn without_a_port_surviving_the_grace_window_is_the_bar() {
        let fake = FakeInstance::default();
        let began = Instant::now();
        assert!(matches!(
            health_gate(&fake, None, GRACE, POLL),
            Health::Healthy
        ));
        assert!(began.elapsed() >= GRACE);
    }

    #[test]
    fn a_port_that_never_answers_reports_the_last_error() {
        let fake = FakeInstance::default();
        match health_gate(&fake, Some(5432), GRACE, POLL) {
            Health::NoAnswer(Some(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused)
            }
            _ => panic!("expected NoAnswer with the connect error"),
        }
    }

    #[test]
    fn a_compliant_instance_gets_only_the_stop_signal() {
        let mut fake = FakeInstance {
            obeys: Some(Signal::SIGTERM),
            ..Default::default()
        };
        let code = stop_with_patience(&mut fake, Signal::SIGTERM, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(0));
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGTERM]);
    }

    #[test]
    fn a_stubborn_instance_is_killed_once_patience_runs_out() {
        let mut fake = FakeInstance::default();
        let code = stop_with_patience(&mut fake, Signal::SIGTERM, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(137));
        assert_eq!(
            *fake.signals.borrow(),
            vec![Signal::SIGTERM, Signal::SIGKILL]
        );
    }

    #[test]
    fn a_shutdown_reaches_an_instance_that_is_not_a_child_process() {
        // A microVM has no pid to kill: `child_pid()` is `None`, so the
        // signal HANDLER cannot reach it and the main loop must. Before
        // this, such an instance got nothing for ten seconds and then
        // SIGKILL — `^C` on `ply run` returned 255 instead of the app's own
        // code, and the guest never saw the signal at all.
        let fake = FakeInstance {
            obeys: Some(Signal::SIGTERM),
            ..Default::default()
        };
        assert_eq!(request_stop(&[&fake as &dyn Instance], Signal::SIGTERM), 1);
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGTERM]);
    }

    #[test]
    fn an_instance_the_handler_already_signalled_is_not_signalled_twice() {
        // A namespace instance IS a child process, and the handler killed it
        // the moment the signal arrived. Signalling it again here would send
        // a second SIGQUIT to an nginx that is already draining.
        let child = FakeInstance {
            child_pid: Some(4243),
            ..Default::default()
        };
        let vm = FakeInstance::default();
        assert_eq!(
            request_stop(
                &[&child as &dyn Instance, &vm as &dyn Instance],
                Signal::SIGQUIT
            ),
            1,
            "only the instance with no child pid is reached from here"
        );
        assert!(child.signals.borrow().is_empty());
        assert_eq!(*vm.signals.borrow(), vec![Signal::SIGQUIT]);
    }

    #[test]
    fn the_declared_stop_signal_is_what_is_sent() {
        let mut fake = FakeInstance {
            obeys: Some(Signal::SIGQUIT),
            exit: 3,
            ..Default::default()
        };
        let code = stop_with_patience(&mut fake, Signal::SIGQUIT, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(3));
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGQUIT]);
    }
}
