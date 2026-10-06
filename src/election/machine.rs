//! The VRRP-style election as an event-driven state machine (spec §5).

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use super::timers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Backup,
    Master,
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Master,
    Backup,
    Fault,
    Stop,
}

/// What the health checks decided: the effective priority, and whether a weight-0 check is failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub effective_priority: u8,
    pub fault: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Started { health: Health },
    Heartbeat { from: Ipv4Addr, priority: u8, interval: Duration },
    TimerFired,
    HealthChanged(Health),
    AttachFailed,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Send a heartbeat to every peer. Priority 0 means goodbye.
    SendHeartbeat {
        priority: u8,
    },
    AttachVips,
    Announce,
    DetachVips,
    RunHook(HookKind),
}

#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub preempt: bool,
    pub advert_interval: Duration,
    pub own_ip: Ipv4Addr,
    pub hold_down: Duration,
    pub reannounce_after: Duration,
}

impl MachineConfig {
    /// Production timings: a 10 s hold-down after an attach failure, and a second
    /// announcement 5 s after becoming master.
    pub fn new(preempt: bool, advert_interval: Duration, own_ip: Ipv4Addr) -> Self {
        Self {
            preempt,
            advert_interval,
            own_ip,
            hold_down: Duration::from_secs(10),
            reannounce_after: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub struct Machine {
    cfg: MachineConfig,
    state: State,
    health: Health,
    started: bool,
    stopped: bool,
    /// The interval used for the down timer: learned from the master's heartbeats.
    master_interval: Duration,
    down_at: Option<Instant>,
    advert_at: Option<Instant>,
    reannounce_at: Option<Instant>,
    hold_down_until: Option<Instant>,
    /// Set by a failed attach. Until this node next becomes master on its own, a lower master's
    /// heartbeats keep it a backup, so a node that cannot hold the VIP cannot keep taking it away
    /// from one that can.
    preempt_suspended: bool,
}

impl Machine {
    pub fn new(cfg: MachineConfig) -> Self {
        let master_interval = cfg.advert_interval;
        Self {
            cfg,
            state: State::Backup,
            health: Health { effective_priority: 1, fault: false },
            started: false,
            stopped: false,
            master_interval,
            down_at: None,
            advert_at: None,
            reannounce_at: None,
            hold_down_until: None,
            preempt_suspended: false,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn health(&self) -> Health {
        self.health
    }

    /// The earliest moment at which the runtime must deliver `Event::TimerFired`.
    pub fn next_deadline(&self) -> Option<Instant> {
        [self.down_at, self.advert_at, self.reannounce_at, self.hold_down_until].into_iter().flatten().min()
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.stopped {
            return actions;
        }
        if !self.started {
            if let Event::Started { health } = event {
                self.started = true;
                self.health = health;
                if health.fault {
                    self.enter_fault(&mut actions);
                } else {
                    self.state = State::Backup;
                    self.arm_down(now);
                }
            }
            return actions;
        }
        match event {
            Event::Started { .. } => {}
            Event::Heartbeat { from, priority, interval } => {
                self.on_heartbeat(from, priority, interval, now, &mut actions)
            }
            Event::TimerFired => self.on_timer(now, &mut actions),
            Event::HealthChanged(health) => self.on_health(health, now, &mut actions),
            Event::AttachFailed => {
                if self.state == State::Master {
                    self.leave_master_for_fault(&mut actions);
                    self.hold_down_until = Some(now + self.cfg.hold_down);
                    self.preempt_suspended = true;
                }
            }
            Event::Shutdown => {
                if self.state == State::Master {
                    actions.push(Action::SendHeartbeat { priority: 0 });
                    actions.push(Action::DetachVips);
                }
                actions.push(Action::RunHook(HookKind::Stop));
                self.stopped = true;
                self.clear_timers();
            }
        }
        actions
    }

    fn mine(&self) -> u8 {
        self.health.effective_priority
    }

    fn preempts(&self) -> bool {
        self.cfg.preempt && !self.preempt_suspended
    }

    fn on_heartbeat(
        &mut self,
        from: Ipv4Addr,
        priority: u8,
        interval: Duration,
        now: Instant,
        actions: &mut Vec<Action>,
    ) {
        match self.state {
            State::Fault => {}
            State::Backup => {
                if priority == 0 {
                    self.down_at = Some(now + timers::skew(self.mine(), self.master_interval));
                } else if !self.preempts() || priority >= self.mine() {
                    self.master_interval = interval;
                    self.arm_down(now);
                }
                // Otherwise ignore it: our down timer runs out and we take over (preemption).
            }
            State::Master => {
                if priority == 0 {
                    self.send_advert(now, actions);
                } else if priority > self.mine() || (priority == self.mine() && from > self.cfg.own_ip) {
                    actions.push(Action::DetachVips);
                    actions.push(Action::RunHook(HookKind::Backup));
                    self.state = State::Backup;
                    self.advert_at = None;
                    self.reannounce_at = None;
                    self.master_interval = interval;
                    self.arm_down(now);
                } else {
                    // Split brain: a lower node also thinks it is master. Remind everyone.
                    self.send_advert(now, actions);
                    actions.push(Action::Announce);
                }
            }
        }
    }

    fn on_timer(&mut self, now: Instant, actions: &mut Vec<Action>) {
        match self.state {
            State::Backup => {
                if self.down_at.is_some_and(|at| at <= now) {
                    self.become_master(now, actions);
                }
            }
            State::Master => {
                if self.advert_at.is_some_and(|at| at <= now) {
                    self.send_advert(now, actions);
                }
                if self.reannounce_at.is_some_and(|at| at <= now) {
                    self.reannounce_at = None;
                    actions.push(Action::Announce);
                }
            }
            State::Fault => {
                if self.hold_down_until.is_some_and(|at| at <= now) {
                    self.hold_down_until = None;
                    if !self.health.fault {
                        self.enter_backup(now, actions);
                    }
                }
            }
        }
    }

    fn on_health(&mut self, health: Health, now: Instant, actions: &mut Vec<Action>) {
        self.health = health;
        match self.state {
            State::Backup | State::Master if health.fault => {
                if self.state == State::Master {
                    actions.push(Action::SendHeartbeat { priority: 0 });
                    actions.push(Action::DetachVips);
                }
                self.enter_fault(actions);
            }
            State::Backup | State::Master => {}
            State::Fault => {
                if !health.fault && self.hold_down_until.is_none() {
                    self.enter_backup(now, actions);
                }
            }
        }
    }

    fn become_master(&mut self, now: Instant, actions: &mut Vec<Action>) {
        self.state = State::Master;
        self.down_at = None;
        self.preempt_suspended = false;
        // Heartbeat first, so an old master releases the VIP before we announce it.
        actions.push(Action::SendHeartbeat { priority: self.mine() });
        actions.push(Action::AttachVips);
        actions.push(Action::RunHook(HookKind::Master));
        self.advert_at = Some(now + self.cfg.advert_interval);
        self.reannounce_at = Some(now + self.cfg.reannounce_after);
    }

    fn leave_master_for_fault(&mut self, actions: &mut Vec<Action>) {
        actions.push(Action::SendHeartbeat { priority: 0 });
        actions.push(Action::DetachVips);
        self.enter_fault(actions);
    }

    fn enter_fault(&mut self, actions: &mut Vec<Action>) {
        self.state = State::Fault;
        self.clear_timers();
        actions.push(Action::RunHook(HookKind::Fault));
    }

    fn enter_backup(&mut self, now: Instant, actions: &mut Vec<Action>) {
        self.state = State::Backup;
        actions.push(Action::RunHook(HookKind::Backup));
        self.arm_down(now);
    }

    // Timers re-arm from the `now` of the event being handled, not from the old deadline, so a late
    // TimerFired delays the next one instead of bunching two together.
    fn send_advert(&mut self, now: Instant, actions: &mut Vec<Action>) {
        actions.push(Action::SendHeartbeat { priority: self.mine() });
        self.advert_at = Some(now + self.cfg.advert_interval);
    }

    fn arm_down(&mut self, now: Instant) {
        self.down_at = Some(now + timers::down_interval(self.mine(), self.master_interval));
    }

    fn clear_timers(&mut self) {
        self.down_at = None;
        self.advert_at = None;
        self.reannounce_at = None;
        self.hold_down_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
    const LOWER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const HIGHER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 3);
    const SEC: Duration = Duration::from_secs(1);
    /// 3 × 1 s + (156 / 256) × 1 s: the down timer for priority 100.
    const DOWN_100: Duration = Duration::from_nanos(3_609_375_000);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn healthy(priority: u8) -> Health {
        Health { effective_priority: priority, fault: false }
    }

    fn faulty(priority: u8) -> Health {
        Health { effective_priority: priority, fault: true }
    }

    fn machine(preempt: bool) -> Machine {
        Machine::new(MachineConfig::new(preempt, SEC, ME))
    }

    /// A started backup, and the instant it started.
    fn backup(priority: u8) -> (Machine, Instant) {
        let mut m = machine(true);
        let t0 = Instant::now();
        assert!(m.handle(Event::Started { health: healthy(priority) }, t0).is_empty());
        (m, t0)
    }

    /// A master, and the instant it became master.
    fn master(priority: u8) -> (Machine, Instant) {
        let (mut m, _) = backup(priority);
        let t = m.next_deadline().unwrap();
        m.handle(Event::TimerFired, t);
        assert_eq!(m.state(), State::Master);
        (m, t)
    }

    fn hb(from: Ipv4Addr, priority: u8) -> Event {
        Event::Heartbeat { from, priority, interval: SEC }
    }

    #[test]
    fn starts_as_backup_with_a_full_down_timer() {
        let (m, t0) = backup(100);
        assert_eq!(m.state(), State::Backup);
        assert_eq!(m.next_deadline(), Some(t0 + DOWN_100));
    }

    #[test]
    fn starts_in_fault_when_health_is_faulty() {
        let mut m = machine(true);
        let actions = m.handle(Event::Started { health: faulty(100) }, Instant::now());
        assert_eq!(actions, vec![Action::RunHook(HookKind::Fault)]);
        assert_eq!(m.state(), State::Fault);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn ignores_events_before_started() {
        let mut m = machine(true);
        assert!(m.handle(Event::TimerFired, Instant::now()).is_empty());
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn backup_becomes_master_when_the_down_timer_expires() {
        let (mut m, _) = backup(100);
        let deadline = m.next_deadline().unwrap();
        assert!(m.handle(Event::TimerFired, deadline - ms(1)).is_empty());
        assert_eq!(m.state(), State::Backup);
        let actions = m.handle(Event::TimerFired, deadline);
        assert_eq!(
            actions,
            vec![Action::SendHeartbeat { priority: 100 }, Action::AttachVips, Action::RunHook(HookKind::Master),]
        );
        assert_eq!(m.state(), State::Master);
        assert_eq!(m.next_deadline(), Some(deadline + SEC));
    }

    #[test]
    fn master_sends_a_heartbeat_every_interval() {
        let (mut m, t) = master(100);
        assert_eq!(m.handle(Event::TimerFired, t + SEC), vec![Action::SendHeartbeat { priority: 100 }]);
        assert_eq!(m.next_deadline(), Some(t + SEC * 2));
    }

    #[test]
    fn master_reannounces_once_five_seconds_after_taking_over() {
        let (mut m, t) = master(100);
        for n in 1..=4 {
            assert_eq!(m.handle(Event::TimerFired, t + SEC * n), vec![Action::SendHeartbeat { priority: 100 }]);
        }
        assert_eq!(
            m.handle(Event::TimerFired, t + SEC * 5),
            vec![Action::SendHeartbeat { priority: 100 }, Action::Announce]
        );
        assert_eq!(m.handle(Event::TimerFired, t + SEC * 6), vec![Action::SendHeartbeat { priority: 100 }]);
    }

    #[test]
    fn backup_resets_its_timer_on_equal_or_higher_heartbeats() {
        let (mut m, t0) = backup(100);
        let later = t0 + ms(2000);
        assert!(m.handle(hb(HIGHER_IP, 100), later).is_empty());
        assert_eq!(m.next_deadline(), Some(later + DOWN_100));
        assert!(m.handle(hb(HIGHER_IP, 200), later + ms(500)).is_empty());
        assert_eq!(m.next_deadline(), Some(later + ms(500) + DOWN_100));
    }

    #[test]
    fn backup_ignores_lower_heartbeats_and_takes_over() {
        let (mut m, t0) = backup(100);
        let deadline = m.next_deadline().unwrap();
        assert!(m.handle(hb(HIGHER_IP, 50), t0 + ms(1000)).is_empty());
        assert_eq!(m.next_deadline(), Some(deadline));
        assert_eq!(m.handle(Event::TimerFired, deadline)[1], Action::AttachVips);
    }

    #[test]
    fn without_preempt_a_backup_accepts_lower_heartbeats() {
        let mut m = machine(false);
        let t0 = Instant::now();
        m.handle(Event::Started { health: healthy(100) }, t0);
        let later = t0 + ms(1000);
        m.handle(hb(HIGHER_IP, 50), later);
        assert_eq!(m.next_deadline(), Some(later + DOWN_100));
    }

    #[test]
    fn backup_takes_over_after_only_the_skew_on_a_goodbye() {
        let (mut m, t0) = backup(100);
        let t = t0 + ms(1000);
        m.handle(hb(HIGHER_IP, 0), t);
        assert_eq!(m.next_deadline(), Some(t + Duration::from_nanos(609_375_000)));
    }

    #[test]
    fn backup_learns_the_master_interval() {
        let (mut m, t0) = backup(100);
        m.handle(Event::Heartbeat { from: HIGHER_IP, priority: 200, interval: ms(100) }, t0);
        // 3 × 100 ms + (156 / 256) × 100 ms = 360.9375 ms
        assert_eq!(m.next_deadline(), Some(t0 + Duration::from_nanos(360_937_500)));
    }

    #[test]
    fn master_steps_down_for_a_higher_priority() {
        let (mut m, t) = master(100);
        let actions = m.handle(hb(LOWER_IP, 150), t + ms(10));
        assert_eq!(actions, vec![Action::DetachVips, Action::RunHook(HookKind::Backup)]);
        assert_eq!(m.state(), State::Backup);
    }

    #[test]
    fn equal_priority_is_decided_by_the_higher_ip() {
        let (mut m, t) = master(100);
        assert_eq!(m.handle(hb(LOWER_IP, 100), t), vec![Action::SendHeartbeat { priority: 100 }, Action::Announce]);
        assert_eq!(m.state(), State::Master);
        assert_eq!(m.handle(hb(HIGHER_IP, 100), t)[0], Action::DetachVips);
        assert_eq!(m.state(), State::Backup);
    }

    #[test]
    fn master_reasserts_itself_against_a_lower_master() {
        let (mut m, t) = master(150);
        let actions = m.handle(hb(HIGHER_IP, 100), t + ms(10));
        assert_eq!(actions, vec![Action::SendHeartbeat { priority: 150 }, Action::Announce]);
        assert_eq!(m.next_deadline(), Some(t + ms(10) + SEC));
    }

    #[test]
    fn master_answers_a_goodbye_with_a_heartbeat() {
        let (mut m, t) = master(100);
        assert_eq!(m.handle(hb(HIGHER_IP, 0), t), vec![Action::SendHeartbeat { priority: 100 }]);
        assert_eq!(m.state(), State::Master);
    }

    #[test]
    fn health_fault_on_master_says_goodbye_and_detaches() {
        let (mut m, t) = master(100);
        let actions = m.handle(Event::HealthChanged(faulty(100)), t);
        assert_eq!(
            actions,
            vec![Action::SendHeartbeat { priority: 0 }, Action::DetachVips, Action::RunHook(HookKind::Fault),]
        );
        assert_eq!(m.state(), State::Fault);
        assert_eq!(m.next_deadline(), None);
        assert!(m.handle(hb(HIGHER_IP, 50), t).is_empty());
    }

    #[test]
    fn fault_ends_when_health_recovers() {
        let (mut m, t0) = backup(100);
        assert_eq!(m.handle(Event::HealthChanged(faulty(100)), t0), vec![Action::RunHook(HookKind::Fault)]);
        let actions = m.handle(Event::HealthChanged(healthy(100)), t0 + SEC);
        assert_eq!(actions, vec![Action::RunHook(HookKind::Backup)]);
        assert_eq!(m.state(), State::Backup);
        assert_eq!(m.next_deadline(), Some(t0 + SEC + DOWN_100));
    }

    #[test]
    fn a_new_effective_priority_is_used_in_heartbeats() {
        let (mut m, t) = master(150);
        assert!(m.handle(Event::HealthChanged(healthy(90)), t).is_empty());
        assert_eq!(m.handle(Event::TimerFired, t + SEC), vec![Action::SendHeartbeat { priority: 90 }]);
    }

    #[test]
    fn attach_failure_holds_down_then_returns_to_backup() {
        let (mut m, t) = master(100);
        let actions = m.handle(Event::AttachFailed, t);
        assert_eq!(
            actions,
            vec![Action::SendHeartbeat { priority: 0 }, Action::DetachVips, Action::RunHook(HookKind::Fault),]
        );
        assert_eq!(m.next_deadline(), Some(t + Duration::from_secs(10)));
        // Healthy check results during the hold-down do not end it early.
        assert!(m.handle(Event::HealthChanged(healthy(100)), t + SEC).is_empty());
        let actions = m.handle(Event::TimerFired, t + Duration::from_secs(10));
        assert_eq!(actions, vec![Action::RunHook(HookKind::Backup)]);
        assert_eq!(m.state(), State::Backup);
    }

    #[test]
    fn after_a_failed_attach_a_node_stops_preempting_until_it_is_master_again() {
        let (mut m, t) = master(150);
        m.handle(Event::AttachFailed, t);
        let t = t + Duration::from_secs(10);
        assert_eq!(m.handle(Event::TimerFired, t), vec![Action::RunHook(HookKind::Backup)]);
        // A lower master's heartbeats now keep this node a backup.
        for i in 1..=10 {
            assert!(m.handle(hb(LOWER_IP, 100), t + SEC * i).is_empty());
        }
        assert!(m.handle(Event::TimerFired, t + SEC * 12).is_empty());
        assert_eq!(m.state(), State::Backup);
        // That master goes silent, so this node takes over, which restores preemption.
        let silent = m.next_deadline().unwrap();
        m.handle(Event::TimerFired, silent);
        assert_eq!(m.state(), State::Master);
        m.handle(hb(HIGHER_IP, 200), silent + SEC);
        assert_eq!(m.state(), State::Backup);
        let down = m.next_deadline();
        assert!(m.handle(hb(LOWER_IP, 100), silent + SEC * 2).is_empty());
        assert_eq!(m.next_deadline(), down, "a lower master's heartbeat no longer resets the down timer");
    }

    #[test]
    fn hold_down_expiry_keeps_fault_while_unhealthy() {
        let (mut m, t) = master(100);
        m.handle(Event::AttachFailed, t);
        assert!(m.handle(Event::HealthChanged(faulty(100)), t + SEC).is_empty());
        assert!(m.handle(Event::TimerFired, t + Duration::from_secs(10)).is_empty());
        assert_eq!(m.state(), State::Fault);
        assert_eq!(
            m.handle(Event::HealthChanged(healthy(100)), t + Duration::from_secs(11)),
            vec![Action::RunHook(HookKind::Backup)]
        );
    }

    #[test]
    fn late_attach_failure_is_ignored_outside_master() {
        let (mut m, t0) = backup(100);
        assert!(m.handle(Event::AttachFailed, t0).is_empty());
        assert_eq!(m.state(), State::Backup);

        let (mut m, t) = master(100);
        m.handle(Event::HealthChanged(faulty(100)), t);
        assert!(m.handle(Event::AttachFailed, t).is_empty());
        assert_eq!(m.state(), State::Fault);
    }

    #[test]
    fn shutdown_from_master_says_goodbye() {
        let (mut m, t) = master(100);
        assert_eq!(
            m.handle(Event::Shutdown, t),
            vec![Action::SendHeartbeat { priority: 0 }, Action::DetachVips, Action::RunHook(HookKind::Stop),]
        );
        assert_eq!(m.next_deadline(), None);
        assert!(m.handle(Event::TimerFired, t + SEC).is_empty());
    }

    #[test]
    fn shutdown_from_backup_only_runs_the_stop_hook() {
        let (mut m, t0) = backup(100);
        assert_eq!(m.handle(Event::Shutdown, t0), vec![Action::RunHook(HookKind::Stop)]);
    }

    #[test]
    fn shutdown_from_fault_only_runs_the_stop_hook() {
        let (mut m, t) = master(100);
        m.handle(Event::HealthChanged(faulty(100)), t);
        assert_eq!(m.state(), State::Fault);
        assert_eq!(m.handle(Event::Shutdown, t + SEC), vec![Action::RunHook(HookKind::Stop)]);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn a_heartbeat_at_the_down_deadline_wins_in_either_order() {
        // Heartbeat first: it re-arms the down timer, so the timer that fires next does nothing.
        let (mut m, _) = backup(100);
        let due = m.next_deadline().unwrap();
        assert!(m.handle(hb(LOWER_IP, 150), due).is_empty());
        assert!(m.handle(Event::TimerFired, due).is_empty());
        assert_eq!(m.state(), State::Backup);
        // Timer first: the node takes over, then steps down at once for the higher priority.
        let (mut m, _) = backup(100);
        let due = m.next_deadline().unwrap();
        m.handle(Event::TimerFired, due);
        assert_eq!(m.state(), State::Master);
        assert_eq!(m.handle(hb(LOWER_IP, 150), due), vec![Action::DetachVips, Action::RunHook(HookKind::Backup)]);
        assert_eq!(m.state(), State::Backup);
    }
}
